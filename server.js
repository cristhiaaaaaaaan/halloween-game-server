/**
 * Halloween Game - Backend WebSocket + MongoDB
 * Reemplaza el servidor Rust para cloud hosting (Railway)
 * Compatible con el protocolo existente del frontend React Native
 */

const { WebSocketServer } = require('ws');
const { MongoClient }     = require('mongodb');

// ─── Configuración ───────────────────────────────────────────────────────────
const PORT       = process.env.PORT || 5000;
const MONGO_URI  = process.env.MONGO_URI || 'mongodb://localhost:27017';
const DB_NAME    = 'halloween_game';

// ─── Armas disponibles y su daño ─────────────────────────────────────────────
const ARMAS = {
  daga:     3,
  granada:  8,
  dinamita: 15,
};

// ─── Monstruos iniciales ──────────────────────────────────────────────────────
const MONSTRUOS_BASE = [
  { id: 'm1', nombre: 'Drácula',    emoji: '🧛', hp_max: 50 },
  { id: 'm2', nombre: 'Frankenstein',emoji: '🧟', hp_max: 70 },
  { id: 'm3', nombre: 'La Momia',   emoji: '🏺', hp_max: 40 },
  { id: 'm4', nombre: 'El Fantasma',emoji: '👻', hp_max: 35 },
  { id: 'm5', nombre: 'La Bruja',   emoji: '🧙', hp_max: 45 },
];

// ─── Configuración de turnos ──────────────────────────────────────────────────
const TURNO_SEGUNDOS = 20; // segundos por turno antes de pasar automáticamente

// ─── Estado del juego ─────────────────────────────────────────────────────────
let jugadores   = [];   // { id, nombre, hp, puntos, armas: [] }
let monstruos   = [];   // copia con hp_actual
let alianzas    = {};   // { alias: [id1, id2, id3] }
let turnoActual = null;
let fase        = 'espera';
let sesionId    = null;
let turnoTimer  = null; // referencia al setTimeout del turno activo

// ─── Helpers ──────────────────────────────────────────────────────────────────

/** Reinicia el estado completo del juego */
function resetEstado() {
  detenerTemporizador();
  jugadores   = [];
  monstruos   = [];
  alianzas    = {};
  turnoActual = null;
  fase        = 'espera';
  sesionId    = null;
}

/** Cancela el temporizador de turno activo */
function detenerTemporizador() {
  if (turnoTimer) {
    clearTimeout(turnoTimer);
    turnoTimer = null;
  }
}

/** Inicia el temporizador del turno actual; si expira, pasa al siguiente */
function iniciarTemporizador(wss) {
  detenerTemporizador();
  if (fase !== 'juego') return;
  turnoTimer = setTimeout(() => {
    console.log(`⏱️ Turno expirado para ${turnoActual} — pasando al siguiente`);
    siguienteTurno();
    broadcast(wss, estadoJuego());
    iniciarTemporizador(wss); // inicia el temporizador del nuevo turno
  }, TURNO_SEGUNDOS * 1000);
}

/** Devuelve el jugador con ese ws como socket */
function jugadorDeSocket(socket) {
  return jugadores.find(j => j._socket === socket);
}

/** Avanza al siguiente turno circular entre jugadores vivos (hp > 0) */
function siguienteTurno() {
  const vivos = jugadores.filter(j => j.hp > 0);
  if (vivos.length === 0) return;
  if (!turnoActual) {
    turnoActual = vivos[0].id;
    return;
  }
  const idx = vivos.findIndex(j => j.id === turnoActual);
  turnoActual = vivos[(idx + 1) % vivos.length].id;
}

/** Comprueba si todos los monstruos están muertos */
function juegoTerminado() {
  return monstruos.every(m => m.hp_actual <= 0);
}

/** Jugador con más puntos */
function calcularGanador() {
  if (jugadores.length === 0) return 'Nadie';
  return jugadores.reduce((a, b) => (a.puntos >= b.puntos ? a : b)).nombre;
}

/** Broadcast a todos los clientes conectados */
function broadcast(wss, mensaje) {
  const txt = JSON.stringify(mensaje);
  wss.clients.forEach(c => {
    if (c.readyState === 1) c.send(txt);
  });
}

/** Estado completo para enviar a todos */
function estadoJuego() {
  return {
    tipo:         'estado_juego',
    jugadores:    jugadores.map(limpiarJugador),
    monstruos,
    turno_actual: turnoActual,
    fase,
    alianzas,
    tiempo_turno: TURNO_SEGUNDOS,
  };
}

/** Quita la referencia al socket antes de enviar al cliente */
function limpiarJugador(j) {
  const { _socket, ...rest } = j;
  return rest;
}

// ─── MongoDB ──────────────────────────────────────────────────────────────────
let db = null;

async function conectarMongo() {
  try {
    const client = new MongoClient(MONGO_URI);
    await client.connect();
    db = client.db(DB_NAME);
    console.log('✅ MongoDB conectado');
  } catch (err) {
    console.error('⚠️  MongoDB no disponible, continuando sin DB:', err.message);
  }
}

async function registrarMovimiento(tipo, datos) {
  if (!db) return;
  try {
    await db.collection('movimientos').insertOne({
      sesion_id:  sesionId,
      tipo,
      datos,
      timestamp:  new Date(),
    });
  } catch (e) { /* silencioso */ }
}

async function registrarSesion(evento, extra = {}) {
  if (!db) return;
  try {
    if (evento === 'inicio') {
      const res = await db.collection('sesiones').insertOne({
        inicio:     new Date(),
        jugadores:  jugadores.map(j => j.nombre),
        ...extra,
      });
      sesionId = res.insertedId;
    } else if (evento === 'fin') {
      await db.collection('sesiones').updateOne(
        { _id: sesionId },
        { $set: { fin: new Date(), ganador: extra.ganador, jugadores_final: extra.jugadores } }
      );
    }
  } catch (e) { /* silencioso */ }
}

// ─── Lógica de mensajes ───────────────────────────────────────────────────────

function manejarUnirse(wss, socket, data) {
  // ID basado en contador simple (Railway no expone ip:port del cliente fácilmente)
  const id = `j${Date.now()}_${Math.random().toString(36).slice(2, 6)}`;
  const jugador = {
    id,
    nombre: data.nombre,
    hp:     10,
    puntos: 0,
    armas:  ['daga'],
    _socket: socket,
  };
  jugadores.push(jugador);

  // Confirmar al nuevo jugador su propio objeto
  socket.send(JSON.stringify({
    tipo:      'jugador_unido',
    jugador:   limpiarJugador(jugador),
    jugadores: jugadores.map(limpiarJugador),
  }));

  // Notificar al resto
  wss.clients.forEach(c => {
    if (c !== socket && c.readyState === 1) {
      c.send(JSON.stringify({
        tipo:      'jugador_unido',
        jugador:   limpiarJugador(jugador),
        jugadores: jugadores.map(limpiarJugador),
      }));
    }
  });

  registrarMovimiento('unirse', { jugador_id: id, nombre: data.nombre });
}

function manejarIniciarJuego(wss) {
  if (jugadores.length < 2) return;
  fase      = 'juego';
  monstruos = MONSTRUOS_BASE.map(m => ({ ...m, hp_actual: m.hp_max }));
  turnoActual = jugadores[0].id;

  registrarSesion('inicio');

  broadcast(wss, {
    tipo:         'juego_iniciado',
    jugadores:    jugadores.map(limpiarJugador),
    monstruos,
    turno_actual: turnoActual,
    tiempo_turno: TURNO_SEGUNDOS,
  });

  iniciarTemporizador(wss);
}

function manejarAlianza(wss, socket, data) {
  const jugador = jugadorDeSocket(socket);
  if (!jugador) return;

  alianzas[data.alias] = data.miembros;
  broadcast(wss, { tipo: 'alianza_formada', alianzas });
  registrarMovimiento('alianza', { alias: data.alias, miembros: data.miembros });
}

async function manejarAtacar(wss, socket, data) {
  const jugador = jugadorDeSocket(socket);
  if (!jugador || jugador.id !== turnoActual) return;
  if (fase !== 'juego') return;

  const arma     = data.arma;
  const dano     = ARMAS[arma];
  if (!dano) return;                                    // arma inválida

  // Verificar que el jugador tiene esa arma
  if (!jugador.armas.includes(arma)) return;

  const monstruo = monstruos.find(m => m.id === data.monstruo_id && m.hp_actual > 0);
  if (!monstruo) return;

  monstruo.hp_actual = Math.max(0, monstruo.hp_actual - dano);

  broadcast(wss, {
    tipo:        'ataque',
    monstruo:    monstruo.nombre,
    atacante:    jugador.nombre,
    arma,
    dano,
    hp_restante: monstruo.hp_actual,
  });

  registrarMovimiento('atacar', {
    jugador_id:  jugador.id,
    monstruo_id: monstruo.id,
    arma,
    dano,
  });

  // Monstruo eliminado
  if (monstruo.hp_actual === 0) {
    const puntos = Math.floor(monstruo.hp_max / 5);
    jugador.puntos += puntos;
    jugador.hp     += 2;
    // Dar granada como recompensa (si no la tiene ya)
    if (!jugador.armas.includes('granada')) jugador.armas.push('granada');

    broadcast(wss, {
      tipo:          'monstruo_eliminado',
      monstruo:      monstruo.nombre,
      eliminado_por: jugador.nombre,
      puntos_ganados: puntos,
    });

    registrarMovimiento('monstruo_eliminado', {
      monstruo_id:   monstruo.id,
      jugador_id:    jugador.id,
      puntos_ganados: puntos,
    });
  }

  // ¿Fin del juego?
  if (juegoTerminado()) {
    fase = 'fin';
    const ganador = calcularGanador();

    await registrarSesion('fin', {
      ganador,
      jugadores: jugadores.map(j => ({ nombre: j.nombre, puntos: j.puntos })),
    });

    broadcast(wss, {
      tipo:      'fin_juego',
      ganador,
      jugadores: jugadores.map(limpiarJugador),
    });
    return;
  }

  // Avanzar turno y enviar estado
  siguienteTurno();
  broadcast(wss, estadoJuego());
  iniciarTemporizador(wss);
}

function manejarIntercambiar(wss, socket, data) {
  const jugador = jugadorDeSocket(socket);
  if (!jugador) return;

  const destino = jugadores.find(j => j.id === data.para);
  if (!destino) {
    socket.send(JSON.stringify({ tipo: 'intercambio', exitoso: false, jugadores: jugadores.map(limpiarJugador) }));
    return;
  }

  // Verificar que son de la misma alianza
  const mismaAlianza = Object.values(alianzas).some(
    miembros => miembros.includes(jugador.id) && miembros.includes(destino.id)
  );
  if (!mismaAlianza || jugador.puntos < data.cantidad) {
    socket.send(JSON.stringify({ tipo: 'intercambio', exitoso: false, jugadores: jugadores.map(limpiarJugador) }));
    return;
  }

  jugador.puntos  -= data.cantidad;
  destino.puntos  += data.cantidad;

  registrarMovimiento('intercambiar', {
    de:       jugador.id,
    para:     destino.id,
    cantidad: data.cantidad,
  });

  broadcast(wss, {
    tipo:      'intercambio',
    exitoso:   true,
    jugadores: jugadores.map(limpiarJugador),
  });
}

function manejarDesconexion(wss, socket) {
  const jugador = jugadorDeSocket(socket);
  if (!jugador) return;

  jugadores = jugadores.filter(j => j.id !== jugador.id);
  console.log(`👋 ${jugador.nombre} desconectado. Jugadores: ${jugadores.length}`);

  // Si no quedan jugadores, resetear
  if (jugadores.length === 0) {
    resetEstado();
    console.log('🔄 Juego reseteado (sin jugadores)');
    return;
  }

  // Si queda solo 1 jugador durante una partida, ese jugador gana
  if (fase === 'juego' && jugadores.length === 1) {
    detenerTemporizador();
    fase = 'fin';
    const ganador = jugadores[0].nombre;
    registrarSesion('fin', {
      ganador,
      jugadores: jugadores.map(j => ({ nombre: j.nombre, puntos: j.puntos })),
    });
    broadcast(wss, {
      tipo:      'fin_juego',
      ganador,
      jugadores: jugadores.map(limpiarJugador),
    });
    console.log(`🏆 ${ganador} gana por abandono del rival`);
    return;
  }

  // Si era su turno, avanzar
  if (turnoActual === jugador.id && fase === 'juego') {
    siguienteTurno();
    iniciarTemporizador(wss);
  }

  broadcast(wss, estadoJuego());
}

// ─── Arranque ─────────────────────────────────────────────────────────────────

(async () => {
  await conectarMongo();

  const wss = new WebSocketServer({ port: PORT });
  console.log(`🎃 Halloween Game WS Server corriendo en puerto ${PORT}`);

  wss.on('connection', (socket) => {
    console.log('🔌 Nueva conexión');

    socket.on('message', async (raw) => {
      try {
        const data = JSON.parse(raw.toString());
        switch (data.tipo) {
          case 'unirse':        manejarUnirse(wss, socket, data);        break;
          case 'iniciar_juego': manejarIniciarJuego(wss);                break;
          case 'alianza':       manejarAlianza(wss, socket, data);       break;
          case 'atacar':        await manejarAtacar(wss, socket, data);  break;
          case 'intercambiar':  manejarIntercambiar(wss, socket, data);  break;
        }
      } catch (e) {
        console.error('Error procesando mensaje:', e.message);
      }
    });

    socket.on('close', () => manejarDesconexion(wss, socket));
    socket.on('error', (e) => console.error('Socket error:', e.message));
  });
})();
