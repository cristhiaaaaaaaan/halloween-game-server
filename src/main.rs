#![allow(dead_code)]

use base64::Engine;
use mongodb::bson::{doc, DateTime as BsonDateTime};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

// ─────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────

const TURNO_SEGUNDOS: u64 = 20;
const GRACIA_SEGUNDOS: u64 = 15;

// ─────────────────────────────────────────────
// Data structures
// ─────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Jugador {
    id: String,
    nombre: String,
    puntos: i32,
    hp: i32,
    armas: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Monstruo {
    id: String,
    nombre: String,
    emoji: String,
    hp_max: i32,
    hp_actual: i32,
    eliminado: bool,
}

struct EstadoJuego {
    sesion_id: String,
    fase: String,
    turno_actual: String,
    orden_turnos: Vec<String>,
    turno_index: usize,
    jugadores: HashMap<String, Jugador>,
    addr_to_id: HashMap<String, String>,
    monstruos: Vec<Monstruo>,
    alianzas: HashMap<String, Vec<String>>,
    turno_reset: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    desconectados: HashMap<String, (Jugador, String)>,
}

type Clients = Arc<Mutex<HashMap<String, UnboundedSender<String>>>>;
type GameState = Arc<tokio::sync::Mutex<EstadoJuego>>;
type MongoDb = Arc<mongodb::Client>;

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn calcular_dano(dano_base: i32) -> (i32, bool) {
    let mut rng = rand::thread_rng();
    let variacion: f64 = rng.gen_range(0.80..=1.20);
    let dano_final = ((dano_base as f64 * variacion).round() as i32).max(1);
    let critico = variacion >= 1.15;
    (dano_final, critico)
}

fn generar_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    format!("j{}_{}", t.as_millis(), rand::thread_rng().gen::<u16>())
}

fn generar_sesion_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    format!("s{}", t.as_millis())
}

fn dano_base_arma(arma: &str) -> Option<i32> {
    match arma {
        "daga" => Some(3),
        "granada" => Some(8),
        "dinamita" => Some(15),
        _ => None,
    }
}

fn crear_monstruos() -> Vec<Monstruo> {
    vec![
        Monstruo { id: "m1".into(), nombre: "Drácula".into(),      emoji: "🧛".into(), hp_max: 30, hp_actual: 30, eliminado: false },
        Monstruo { id: "m2".into(), nombre: "Frankenstein".into(),  emoji: "🧟".into(), hp_max: 40, hp_actual: 40, eliminado: false },
        Monstruo { id: "m3".into(), nombre: "La Momia".into(),      emoji: "🏺".into(), hp_max: 25, hp_actual: 25, eliminado: false },
        Monstruo { id: "m4".into(), nombre: "El Fantasma".into(),   emoji: "👻".into(), hp_max: 20, hp_actual: 20, eliminado: false },
        Monstruo { id: "m5".into(), nombre: "La Bruja".into(),      emoji: "🧙".into(), hp_max: 28, hp_actual: 28, eliminado: false },
    ]
}

// ─────────────────────────────────────────────
// Broadcast helpers
// ─────────────────────────────────────────────

fn broadcast_str(clients: &Clients, msg: &str) {
    let map = clients.lock().unwrap();
    for tx in map.values() {
        let _ = tx.send(msg.to_string());
    }
}

fn send_to_addr(clients: &Clients, addr: &str, msg: &str) {
    let map = clients.lock().unwrap();
    if let Some(tx) = map.get(addr) {
        let _ = tx.send(msg.to_string());
    }
}

// ─────────────────────────────────────────────
// State snapshot
// ─────────────────────────────────────────────

fn build_estado_snap(state: &EstadoJuego) -> String {
    let jugadores: Vec<&Jugador> = state.jugadores.values().collect();
    json!({
        "tipo": "estado_juego",
        "jugadores": jugadores,
        "monstruos": state.monstruos,
        "turno_actual": state.turno_actual,
        "fase": state.fase,
        "alianzas": state.alianzas,
        "tiempo_turno": TURNO_SEGUNDOS,
    })
    .to_string()
}

// ─────────────────────────────────────────────
// Turn advancement
// ─────────────────────────────────────────────

fn siguiente_turno(state: &mut EstadoJuego) {
    if state.orden_turnos.is_empty() {
        return;
    }
    state.turno_index = (state.turno_index + 1) % state.orden_turnos.len();
    // Skip disconnected players
    let mut attempts = 0;
    while attempts < state.orden_turnos.len() {
        let id = &state.orden_turnos[state.turno_index].clone();
        if state.jugadores.contains_key(id) {
            state.turno_actual = id.clone();
            return;
        }
        state.turno_index = (state.turno_index + 1) % state.orden_turnos.len();
        attempts += 1;
    }
}

// ─────────────────────────────────────────────
// Turn timer
// ─────────────────────────────────────────────

fn iniciar_turno_timer(gs: GameState, clients: Clients) -> tokio::sync::mpsc::UnboundedSender<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(TURNO_SEGUNDOS)) => {
                    let mut state = gs.lock().await;
                    if state.fase != "juego" { break; }
                    siguiente_turno(&mut state);
                    let snap = build_estado_snap(&state);
                    drop(state);
                    broadcast_str(&clients, &snap);
                }
                msg = rx.recv() => {
                    if msg.is_none() { break; }
                    // reset — loop restarts the sleep
                }
            }
        }
    });
    tx
}

// ─────────────────────────────────────────────
// Reset game
// ─────────────────────────────────────────────

fn reset_juego(state: &mut EstadoJuego, grace_handles: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>) {
    // Abort all grace timers
    let mut handles = grace_handles.lock().unwrap();
    for (_, handle) in handles.drain() {
        handle.abort();
    }
    drop(handles);

    state.turno_reset = None;
    state.fase = "espera".into();
    state.turno_actual = String::new();
    state.orden_turnos.clear();
    state.turno_index = 0;
    state.monstruos.clear();
    state.alianzas.clear();
    state.desconectados.clear();
    state.jugadores.clear();
    state.addr_to_id.clear();
    state.sesion_id = generar_sesion_id();
}

// ─────────────────────────────────────────────
// MongoDB helpers
// ─────────────────────────────────────────────

async fn log_movimiento(db: &MongoDb, sesion_id: &str, tipo: &str, datos: &str) {
    let col = db
        .database("halloween_game")
        .collection::<mongodb::bson::Document>("movimientos");
    let doc = doc! {
        "sesion_id": sesion_id,
        "tipo": tipo,
        "datos": datos,
        "timestamp": BsonDateTime::now(),
    };
    let _ = col.insert_one(doc).await;
}

async fn log_sesion_inicio(db: &MongoDb, sesion_id: &str, nombres: Vec<String>) {
    let col = db
        .database("halloween_game")
        .collection::<mongodb::bson::Document>("sesiones");
    let nombres_bson: Vec<mongodb::bson::Bson> = nombres
        .iter()
        .map(|n| mongodb::bson::Bson::String(n.clone()))
        .collect();
    let doc = doc! {
        "sesion_id": sesion_id,
        "inicio": BsonDateTime::now(),
        "jugadores": nombres_bson,
    };
    let _ = col.insert_one(doc).await;
}

async fn log_sesion_fin(db: &MongoDb, sesion_id: &str, ganador: &str, jugadores_final: &[Jugador]) {
    let col = db
        .database("halloween_game")
        .collection::<mongodb::bson::Document>("sesiones");
    let jugadores_bson: Vec<mongodb::bson::Bson> = jugadores_final
        .iter()
        .map(|j| {
            mongodb::bson::Bson::Document(doc! {
                "id": &j.id,
                "nombre": &j.nombre,
                "puntos": j.puntos,
                "hp": j.hp,
            })
        })
        .collect();
    let _ = col
        .update_one(
            doc! { "sesion_id": sesion_id },
            doc! { "$set": { "fin": BsonDateTime::now(), "ganador": ganador, "jugadores_final": jugadores_bson } },
        )
        .await;
}

// ─────────────────────────────────────────────
// WebSocket handshake & frame I/O
// ─────────────────────────────────────────────

async fn ws_handshake(stream: &mut TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let key = request
        .lines()
        .find(|l| l.to_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .map(|s| s.trim())
        .ok_or("no sec-websocket-key")?;

    let magic = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let concat = format!("{}{}", key, magic);
    let mut hasher = Sha1::new();
    hasher.update(concat.as_bytes());
    let hash = hasher.finalize();
    let accept = base64::engine::general_purpose::STANDARD.encode(hash);

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Read one WebSocket frame. Returns (opcode, payload) or error.
async fn ws_read_frame<R>(stream: &mut R) -> Result<(u8, Vec<u8>), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin,
{
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;

    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut payload_len = (header[1] & 0x7F) as u64;

    if payload_len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).await?;
        payload_len = u16::from_be_bytes(ext) as u64;
    } else if payload_len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).await?;
        payload_len = u64::from_be_bytes(ext);
    }

    let mask = if masked {
        let mut m = [0u8; 4];
        stream.read_exact(&mut m).await?;
        Some(m)
    } else {
        None
    };

    let mut payload = vec![0u8; payload_len as usize];
    stream.read_exact(&mut payload).await?;

    if let Some(mask) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }

    Ok((opcode, payload))
}

/// Write one WebSocket text frame (unmasked, server → client).
fn ws_make_text_frame(msg: &str) -> Vec<u8> {
    let payload = msg.as_bytes();
    let len = payload.len();
    let mut frame = Vec::new();
    frame.push(0x81u8); // FIN + text opcode
    if len < 126 {
        frame.push(len as u8);
    } else if len < 65536 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// Write pong frame.
fn ws_make_pong_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.push(0x8Au8);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    frame
}

// ─────────────────────────────────────────────
// Per-connection message handler
// ─────────────────────────────────────────────

async fn handle_message(
    addr: &str,
    msg: Value,
    clients: &Clients,
    gs: &GameState,
    grace_handles: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    db: &MongoDb,
) {
    let tipo = match msg.get("tipo").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return,
    };

    match tipo.as_str() {
        // ── unirse ──────────────────────────────────────────────────────────
        "unirse" => {
            let nombre = match msg.get("nombre").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => return,
            };

            let mut state = gs.lock().await;

            // Check reconnection
            if let Some((jugador_snap, _old_addr)) = state.desconectados.remove(&nombre) {
                // Cancel grace timer
                {
                    let mut handles = grace_handles.lock().unwrap();
                    if let Some(h) = handles.remove(&nombre) {
                        h.abort();
                    }
                }

                let jugador_id = jugador_snap.id.clone();
                state.addr_to_id.insert(addr.to_string(), jugador_id.clone());
                state.jugadores.insert(jugador_id, jugador_snap);

                let snap = build_estado_snap(&state);
                let reconectado_msg = json!({
                    "tipo": "jugador_reconectado",
                    "nombre": nombre,
                }).to_string();
                drop(state);

                send_to_addr(clients, addr, &snap);
                broadcast_str(clients, &reconectado_msg);
                return;
            }

            // New player
            let id = generar_id();
            let jugador = Jugador {
                id: id.clone(),
                nombre: nombre.clone(),
                puntos: 0,
                hp: 10,
                armas: vec!["daga".into(), "granada".into(), "dinamita".into()],
            };

            state.jugadores.insert(id.clone(), jugador.clone());
            state.addr_to_id.insert(addr.to_string(), id.clone());

            let jugadores: Vec<&Jugador> = state.jugadores.values().collect();
            let broadcast = json!({
                "tipo": "jugador_unido",
                "jugador": jugador,
                "jugadores": jugadores,
            })
            .to_string();
            let sesion_id = state.sesion_id.clone();
            drop(state);

            broadcast_str(clients, &broadcast);
            log_movimiento(db, &sesion_id, "unirse", &nombre).await;
        }

        // ── iniciar_juego ───────────────────────────────────────────────────
        "iniciar_juego" => {
            let mut state = gs.lock().await;

            if state.jugadores.len() < 2 {
                let err = json!({"tipo":"error","mensaje":"Se necesitan al menos 2 jugadores"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            if state.fase != "espera" {
                let err = json!({"tipo":"error","mensaje":"Juego ya iniciado"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            state.fase = "juego".into();
            state.monstruos = crear_monstruos();
            state.orden_turnos = state.jugadores.keys().cloned().collect();
            state.turno_index = 0;
            state.turno_actual = state.orden_turnos[0].clone();

            let timer_tx = iniciar_turno_timer(gs.clone(), clients.clone());
            state.turno_reset = Some(timer_tx);

            let nombres: Vec<String> = state.jugadores.values().map(|j| j.nombre.clone()).collect();
            let sesion_id = state.sesion_id.clone();

            let broadcast = json!({
                "tipo": "juego_iniciado",
                "monstruos": state.monstruos,
                "turno_actual": state.turno_actual,
                "jugadores": state.jugadores.values().collect::<Vec<_>>(),
                "tiempo_turno": TURNO_SEGUNDOS,
            })
            .to_string();
            drop(state);

            broadcast_str(clients, &broadcast);
            log_sesion_inicio(db, &sesion_id, nombres).await;
            log_movimiento(db, &sesion_id, "iniciar_juego", "").await;
        }

        // ── alianza ─────────────────────────────────────────────────────────
        "alianza" => {
            let alias = match msg.get("alias").and_then(|v| v.as_str()) {
                Some(a) => a.to_string(),
                None => return,
            };
            let miembros: Vec<String> = match msg.get("miembros").and_then(|v| v.as_array()) {
                Some(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect(),
                None => return,
            };

            let mut state = gs.lock().await;
            state.alianzas.insert(alias, miembros);

            let alianzas = state.alianzas.clone();
            let sesion_id = state.sesion_id.clone();
            drop(state);

            let broadcast = json!({
                "tipo": "alianza_formada",
                "alianzas": alianzas,
            })
            .to_string();
            broadcast_str(clients, &broadcast);
            log_movimiento(db, &sesion_id, "alianza", "").await;
        }

        // ── atacar ──────────────────────────────────────────────────────────
        "atacar" => {
            let monstruo_id = match msg.get("monstruo_id").and_then(|v| v.as_str()) {
                Some(m) => m.to_string(),
                None => return,
            };
            let arma = match msg.get("arma").and_then(|v| v.as_str()) {
                Some(a) => a.to_string(),
                None => return,
            };

            let mut state = gs.lock().await;

            // Validate
            if state.fase != "juego" {
                let err = json!({"tipo":"error","mensaje":"Juego no activo"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            let jugador_id = match state.addr_to_id.get(addr).cloned() {
                Some(id) => id,
                None => {
                    let err = json!({"tipo":"error","mensaje":"No estás en el juego"}).to_string();
                    drop(state);
                    send_to_addr(clients, addr, &err);
                    return;
                }
            };

            if state.turno_actual != jugador_id {
                let err = json!({"tipo":"error","mensaje":"No es tu turno"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            let tiene_arma = state
                .jugadores
                .get(&jugador_id)
                .map(|j| j.armas.contains(&arma))
                .unwrap_or(false);

            if !tiene_arma {
                let err = json!({"tipo":"error","mensaje":"No tienes esa arma"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            let dano_base = match dano_base_arma(&arma) {
                Some(d) => d,
                None => {
                    let err = json!({"tipo":"error","mensaje":"Arma inválida"}).to_string();
                    drop(state);
                    send_to_addr(clients, addr, &err);
                    return;
                }
            };

            let (dano, critico) = calcular_dano(dano_base);

            // Find monster
            let monstruo_idx = match state.monstruos.iter().position(|m| m.id == monstruo_id) {
                Some(i) => i,
                None => {
                    let err = json!({"tipo":"error","mensaje":"Monstruo no encontrado"}).to_string();
                    drop(state);
                    send_to_addr(clients, addr, &err);
                    return;
                }
            };

            if state.monstruos[monstruo_idx].eliminado {
                let err = json!({"tipo":"error","mensaje":"Monstruo ya eliminado"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &err);
                return;
            }

            // Apply damage
            let m = &mut state.monstruos[monstruo_idx];
            m.hp_actual = (m.hp_actual - dano).max(0);
            let hp_restante = m.hp_actual;
            let monstruo_nombre = m.nombre.clone();
            let hp_max = m.hp_max;
            let eliminado = hp_restante == 0;
            if eliminado {
                m.eliminado = true;
            }

            let atacante_nombre = state
                .jugadores
                .get(&jugador_id)
                .map(|j| j.nombre.clone())
                .unwrap_or_default();

            let sesion_id = state.sesion_id.clone();

            // If monster eliminated, give rewards
            let puntos_ganados = if eliminado { hp_max / 5 } else { 0 };
            if eliminado {
                if let Some(j) = state.jugadores.get_mut(&jugador_id) {
                    j.puntos += puntos_ganados;
                    j.hp += 2;
                }
            }

            // Build attack broadcast
            let ataque_msg = json!({
                "tipo": "ataque",
                "monstruo": monstruo_nombre,
                "atacante": atacante_nombre,
                "arma": arma,
                "dano": dano,
                "dano_base": dano_base,
                "critico": critico,
                "hp_restante": hp_restante,
            })
            .to_string();

            // Check if all monsters eliminated
            let todos_eliminados = state.monstruos.iter().all(|m| m.eliminado);

            // Reset turn timer
            if let Some(ref tx) = state.turno_reset {
                let _ = tx.send(());
            }

            // Advance turn
            siguiente_turno(&mut state);
            let snap = build_estado_snap(&state);

            // Prepare extra messages
            let eliminado_msg = if eliminado {
                Some(json!({
                    "tipo": "monstruo_eliminado",
                    "monstruo": monstruo_nombre,
                    "eliminado_por": atacante_nombre,
                    "puntos_ganados": puntos_ganados,
                }).to_string())
            } else {
                None
            };

            let fin_msg = if todos_eliminados {
                // Find winner: player with most points
                let ganador = state
                    .jugadores
                    .values()
                    .max_by_key(|j| j.puntos)
                    .cloned();
                let ganador_nombre = ganador.as_ref().map(|j| j.nombre.clone()).unwrap_or_default();
                let jugadores_final: Vec<Jugador> = state.jugadores.values().cloned().collect();
                state.fase = "fin".into();
                state.turno_reset = None; // stop timer

                let fin = json!({
                    "tipo": "fin_juego",
                    "ganador": ganador_nombre,
                    "jugadores": jugadores_final,
                }).to_string();

                // Log to mongo (done outside lock)
                Some((fin, ganador_nombre, jugadores_final))
            } else {
                None
            };

            drop(state);

            broadcast_str(clients, &ataque_msg);
            if let Some(ref em) = eliminado_msg {
                broadcast_str(clients, em);
            }
            if let Some((ref fin, ref ganador_nombre, ref jugadores_final)) = fin_msg {
                broadcast_str(clients, fin);
                log_sesion_fin(db, &sesion_id, ganador_nombre, jugadores_final).await;
            } else {
                broadcast_str(clients, &snap);
            }
            log_movimiento(db, &sesion_id, "atacar", &format!("{} con {}", monstruo_id, arma)).await;
        }

        // ── intercambiar ─────────────────────────────────────────────────────
        "intercambiar" => {
            let para = match msg.get("para").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            let cantidad = match msg.get("cantidad").and_then(|v| v.as_i64()) {
                Some(c) => c as i32,
                None => return,
            };

            let mut state = gs.lock().await;

            let jugador_id = match state.addr_to_id.get(addr).cloned() {
                Some(id) => id,
                None => return,
            };

            // Validate same alliance
            let misma_alianza = state.alianzas.values().any(|miembros| {
                miembros.contains(&jugador_id) && miembros.contains(&para)
            });

            if !misma_alianza {
                let msg_err = json!({"tipo":"intercambio","exitoso":false,"razon":"No son aliados"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &msg_err);
                return;
            }

            // Check funds
            let tiene = state
                .jugadores
                .get(&jugador_id)
                .map(|j| j.puntos >= cantidad)
                .unwrap_or(false);

            if !tiene {
                let msg_err = json!({"tipo":"intercambio","exitoso":false,"razon":"Puntos insuficientes"}).to_string();
                drop(state);
                send_to_addr(clients, addr, &msg_err);
                return;
            }

            if let Some(j) = state.jugadores.get_mut(&jugador_id) {
                j.puntos -= cantidad;
            }
            if let Some(j) = state.jugadores.get_mut(&para) {
                j.puntos += cantidad;
            }

            let jugadores: Vec<&Jugador> = state.jugadores.values().collect();
            let sesion_id = state.sesion_id.clone();
            let broadcast = json!({
                "tipo": "intercambio",
                "exitoso": true,
                "jugadores": jugadores,
            })
            .to_string();
            drop(state);

            broadcast_str(clients, &broadcast);
            log_movimiento(db, &sesion_id, "intercambiar", &format!("{} -> {} puntos", cantidad, para)).await;
        }

        _ => {}
    }
}

// ─────────────────────────────────────────────
// Per-connection handler
// ─────────────────────────────────────────────

async fn handle_connection(
    mut stream: TcpStream,
    addr: String,
    clients: Clients,
    gs: GameState,
    grace_handles: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    db: MongoDb,
) {
    // WebSocket handshake
    if let Err(e) = ws_handshake(&mut stream).await {
        eprintln!("[{}] Handshake failed: {}", addr, e);
        return;
    }

    println!("[{}] WebSocket connected", addr);

    // Create outbound channel
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut map = clients.lock().unwrap();
        map.insert(addr.clone(), out_tx);
    }

    // Send current state snapshot if game is in progress
    {
        let state = gs.lock().await;
        let snap = if !state.jugadores.is_empty() || state.fase != "espera" {
            Some(build_estado_snap(&state))
        } else {
            None
        };
        drop(state);
        if let Some(snap) = snap {
            let map = clients.lock().unwrap();
            if let Some(tx) = map.get(&addr) {
                let _ = tx.send(snap);
            }
        }
    }

    let (mut reader, mut writer) = stream.into_split();

    // Outbound writer task
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let frame = ws_make_text_frame(&msg);
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // Inbound reader loop
    loop {
        let result = ws_read_frame(&mut reader).await;
        match result {
            Ok((opcode, payload)) => {
                match opcode {
                    0x1 => {
                        // Text frame
                        let text = match String::from_utf8(payload) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        handle_message(&addr, parsed, &clients, &gs, &grace_handles, &db).await;
                    }
                    0x8 => {
                        // Close
                        println!("[{}] Close frame received", addr);
                        break;
                    }
                    0x9 => {
                        // Ping received — pong is best-effort; Railway's LB handles keep-alive.
                        // ws_make_pong_frame is available if a bytes channel is added later.
                        let _ = payload;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                eprintln!("[{}] Read error: {}", addr, e);
                break;
            }
        }
    }

    // ── Disconnection logic ───────────────────────────────────────────────
    println!("[{}] Disconnected", addr);
    {
        let mut map = clients.lock().unwrap();
        map.remove(&addr);
    }

    let mut state = gs.lock().await;
    let jugador_id = state.addr_to_id.remove(&addr);

    if let Some(jugador_id) = jugador_id {
        if state.fase == "juego" {
            // Grace period
            if let Some(jugador) = state.jugadores.get(&jugador_id).cloned() {
                let nombre = jugador.nombre.clone();
                state
                    .desconectados
                    .insert(nombre.clone(), (jugador, addr.clone()));

                let desconectado_msg = json!({
                    "tipo": "jugador_desconectado",
                    "nombre": nombre,
                    "gracia": GRACIA_SEGUNDOS,
                })
                .to_string();

                let gs_clone = gs.clone();
                let clients_clone = clients.clone();
                let grace_handles_clone = grace_handles.clone();
                let nombre_clone = nombre.clone();
                let jugador_id_clone = jugador_id.clone();

                let handle = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(GRACIA_SEGUNDOS)).await;

                    let mut state = gs_clone.lock().await;
                    if state.desconectados.contains_key(&nombre_clone) {
                        // Player didn't reconnect — remove them
                        state.desconectados.remove(&nombre_clone);
                        state.jugadores.remove(&jugador_id_clone);

                        // If turn was theirs, advance
                        if state.turno_actual == jugador_id_clone {
                            siguiente_turno(&mut state);
                        }

                        let snap = build_estado_snap(&state);
                        drop(state);
                        broadcast_str(&clients_clone, &snap);
                    }

                    // Clean up handle entry
                    let mut handles = grace_handles_clone.lock().unwrap();
                    handles.remove(&nombre_clone);
                });

                {
                    let mut handles = grace_handles.lock().unwrap();
                    handles.insert(nombre.clone(), handle);
                }

                drop(state);
                broadcast_str(&clients, &desconectado_msg);
            } else {
                drop(state);
            }
        } else {
            // Not in game — remove immediately
            state.jugadores.remove(&jugador_id);
            let snap = if !state.jugadores.is_empty() {
                Some(build_estado_snap(&state))
            } else {
                None
            };
            drop(state);
            if let Some(s) = snap {
                broadcast_str(&clients, &s);
            }
        }
    } else {
        drop(state);
    }
}

// ─────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "5000".to_string());
    let mongo_uri =
        std::env::var("MONGO_URI").unwrap_or_else(|_| "mongodb://localhost:27017".to_string());

    // MongoDB
    let mongo_client = mongodb::Client::with_uri_str(&mongo_uri)
        .await
        .expect("Failed to connect to MongoDB");
    let db: MongoDb = Arc::new(mongo_client);

    // Shared state
    let game_state: GameState = Arc::new(tokio::sync::Mutex::new(EstadoJuego {
        sesion_id: generar_sesion_id(),
        fase: "espera".into(),
        turno_actual: String::new(),
        orden_turnos: Vec::new(),
        turno_index: 0,
        jugadores: HashMap::new(),
        addr_to_id: HashMap::new(),
        monstruos: Vec::new(),
        alianzas: HashMap::new(),
        turno_reset: None,
        desconectados: HashMap::new(),
    }));

    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));
    let grace_handles: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .expect("Failed to bind port");

    println!("Halloween Game Server listening on {}", bind_addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let addr = peer_addr.to_string();
                let clients = clients.clone();
                let gs = game_state.clone();
                let grace_handles = grace_handles.clone();
                let db = db.clone();

                tokio::spawn(async move {
                    handle_connection(stream, addr, clients, gs, grace_handles, db).await;
                });
            }
            Err(e) => {
                eprintln!("Accept error: {}", e);
            }
        }
    }
}
