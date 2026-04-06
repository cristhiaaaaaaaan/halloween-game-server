/**
 * Halloween Game - Backend WebSocket + MongoDB
 * Reemplaza el servidor Rust para cloud hosting (Railway)
 * Compatible con el protocolo existente del frontend React Native
 *
 * Protocolo de mensajes (cliente → servidor):
 *   unirse          { tipo, nombre }
 *   iniciar_juego   { tipo }
 *   alianza         { tipo, alias, miembros }
 *   atacar          { tipo, monstruo_id, arma }
 *   intercambiar    { tipo, para, cantidad }
 *
 * Protocolo de mensajes (servidor → cliente):
 *   jugador_unido   { tipo, jugador, jugadores }
 *   juego_iniciado  { tipo, jugadores, monstruos, turno_actual, tiempo_turno }
 *   alianza_formada { tipo, alianzas }
 *   ataque          { tipo, monstruo, atacante, arma, dano, dano_base, critico, hp_restante }
 *   monstruo_eliminado { tipo, monstruo, eliminado_por, puntos_ganados }
 *   fin_juego       { tipo, ganador, jugadores }
 *   intercambio     { tipo, exitoso, jugadores }
 *   estado_juego    { tipo, jugadores, monstruos, turno_actual, fase, alianzas, tiempo_turno }
 */

const { WebSocketServer } = require('ws');
const { MongoClient }     = require('mongodb');

// ─── Configuración ───────────────────────────────────────────────────────────
const PORT       = process.env.PORT || 5000;
const MONGO_URI  = process.env.MONGO_URI || 'mongodb://localhost:27017';
const DB_NAME    = 'halloween_game';

// ─── Armas disponibles y su daño base ────────────────────────────────────────
const ARMAS = {
  daga:     3,
  granada:  8,
  dinamita: 15,
};

/**
 * Calcula el daño final de un arma aplicando variación aleatoria ±20%.
 * @param {number} danoBase - Daño base del arma
 * @returns {{ danoFinal: number, critico: boolean }} Daño calculado e indicador de crítico
 */
function calcularDano(danoBase) {
  // Variación aleatoria entre -20% y +20%
  const variacion = 1 + (Math.random() * 0.4 - 0.2); // 0.80 a 1.20
  const danoFinal = Math.max(1, Math.round(danoBase * variacion));
  const critico   = variacion >= 1.15; // 15%+ se considera golpe crítico
  return { danoFinal, critico };
}

// ─── Monstruos iniciales ──────────────────────────────────────────────────────
// HP reducido para partidas más ágiles (~3-4 ataques por monstruo con dinamita)
const MONSTRUOS_BASE = [
  { id: 'm1', nombre: 'Drácula',     emoji: '🧛', hp_max: 30 },
  { id: 'm2', nombre: 'Frankenstein',emoji: '🧟', hp_max: 40 },
  { id: 'm3', nombre: 'La Momia',    emoji: '🏺', hp_max: 25 },
  { id: 'm4', nombre: 'El Fantasma', emoji: '👻', hp_max: 20 },
  { id: 'm5', nombre: 'La Bruja',    emoji: '🧙', hp_max: 28 },
];

// ─── Configuración de turnos ──────────────────────────────────────────────────
const TURNO_SEGUNDOS  = 20; // segundos por turno antes de pasar automáticamente
const GRACIA_SEGUNDOS = 15; // segundos de espera antes de eliminar a un jugador desconectado

// ─── Estado del juego ─────────────────────────────────────────────────────────
let jugadores    = [];   // { id, nombre, hp, puntos, armas: [] }
let monstruos    = [];   // copia con hp_actual
let alianzas     = {};   // { alias: [id1, id2, id3] }
let turnoActual  = null;
let fase         = 'espera';
let sesionId     = null;
let turnoTimer   = null; // referencia al setTimeout del turno activo
let graciaTimers = {};   // { jugadorId: timeoutId } — timers de reconexión pendientes

// ─── Helpers ──────────────────────────────────────────────────────────────────

/** Reinicia el estado completo del juego */
function resetEstado() {
  detenerTemporizador();
  Object.values(graciaTimers).forEach(t => clearTimeout(t));
  graciaTimers = {};
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
  // Reconexión: si hay una partida activa y el jugador (por nombre) tiene timer de gracia pendiente
  const reconectado = fase === 'juego'
    ? jugadores.find(j => j.nombre === data.nombre && !j._socket)
    : null;

  if (reconectado) {
    // Cancelar el timer de gracia y restaurar el socket
    clearTimeout(graciaTimers[reconectado.id]);
    delete graciaTimers[reconectado.id];
    reconectado._socket = socket;

    console.log(`🔁 ${reconectado.nombre} reconectó`);

    // Enviarle el estado actual completo para que se sincronice
    socket.send(JSON.stringify({
      tipo:      'jugador_unido',
      jugador:   limpiarJugador(reconectado),
      jugadores: jugadores.map(limpiarJugador),
    }));
    socket.send(JSON.stringify({
      ...estadoJuego(),
      tipo: 'juego_iniciado',
      tiempo_turno: TURNO_SEGUNDOS,
    }));

    broadcast(wss, { tipo: 'jugador_reconectado', nombre: reconectado.nombre });
    return;
  }

  // Nuevo jugador
  const id = `j${Date.now()}_${Math.random().toString(36).slice(2, 6)}`;
  const jugador = {
    id,
    nombre: data.nombre,
    hp:     10,
    puntos: 0,
    armas:  ['daga', 'granada', 'dinamita'], // todos los jugadores empiezan con las 3 armas
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
  const danoBase = ARMAS[arma];
  if (!danoBase) return;                                // arma inválida

  // Verificar que el jugador tiene esa arma
  if (!jugador.armas.includes(arma)) return;

  const monstruo = monstruos.find(m => m.id === data.monstruo_id && m.hp_actual > 0);
  if (!monstruo) return;

  // Aplicar variación aleatoria ±20% al daño base
  const { danoFinal, critico } = calcularDano(danoBase);
  monstruo.hp_actual = Math.max(0, monstruo.hp_actual - danoFinal);

  broadcast(wss, {
    tipo:        'ataque',
    monstruo:    monstruo.nombre,
    atacante:    jugador.nombre,
    arma,
    dano:        danoFinal,
    dano_base:   danoBase,
    critico,
    hp_restante: monstruo.hp_actual,
  });

  registrarMovimiento('atacar', {
    jugador_id:  jugador.id,
    monstruo_id: monstruo.id,
    arma,
    dano_base:   danoBase,
    dano_final:  danoFinal,
    critico,
  });

  // Monstruo eliminado
  if (monstruo.hp_actual === 0) {
    const puntos = Math.floor(monstruo.hp_max / 5);
    jugador.puntos += puntos;
    jugador.hp     += 2;
    // Recompensa por eliminar monstruo: +2 HP y +1 dinamita extra (si no la tiene ya)
    // Todos ya empiezan con las 3 armas, pero este slot queda para futuras mejoras

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

/**
 * Maneja la desconexión de un jugador.
 * Si la partida está en curso, espera GRACIA_SEGUNDOS antes de eliminarlo,
 * por si la app vuelve al frente y reconecta (ej. minimizar en Android).
 * Si no reconecta en ese tiempo, se aplica la regla normal.
 */
function manejarDesconexion(wss, socket) {
  const jugador = jugadorDeSocket(socket);
  if (!jugador) return;

  // Si no hay partida en curso, eliminar inmediatamente (lobby o fin)
  if (fase !== 'juego') {
    jugadores = jugadores.filter(j => j.id !== jugador.id);
    console.log(`👋 ${jugador.nombre} salió del lobby.`);
    if (jugadores.length === 0) resetEstado();
    else broadcast(wss, estadoJuego());
    return;
  }

  // Partida en curso: desasociar el socket pero mantener al jugador en la lista
  jugador._socket = null;
  console.log(`📵 ${jugador.nombre} desconectado. Esperando reconexión (${GRACIA_SEGUNDOS}s)...`);

  // Notificar a los demás que está desconectado temporalmente
  broadcast(wss, {
    tipo:     'jugador_desconectado',
    nombre:   jugador.nombre,
    gracia:   GRACIA_SEGUNDOS,
  });

  // Si era su turno, avanzar para no bloquear la partida
  if (turnoActual === jugador.id) {
    siguienteTurno();
    broadcast(wss, estadoJuego());
    iniciarTemporizador(wss);
  }

  // Timer de gracia: si no reconecta, eliminar definitivamente
  graciaTimers[jugador.id] = setTimeout(() => {
    delete graciaTimers[jugador.id];

    // Verificar que sigue sin socket (no reconectó)
    const aun = jugadores.find(j => j.id === jugador.id);
    if (!aun || aun._socket) return; // ya reconectó, no hacer nada

    jugadores = jugadores.filter(j => j.id !== jugador.id);
    console.log(`👋 ${jugador.nombre} eliminado por no reconectar. Jugadores: ${jugadores.length}`);

    if (jugadores.length === 0) {
      resetEstado();
      console.log('🔄 Juego reseteado (sin jugadores)');
      return;
    }

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

    broadcast(wss, estadoJuego());
  }, GRACIA_SEGUNDOS * 1000);
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
