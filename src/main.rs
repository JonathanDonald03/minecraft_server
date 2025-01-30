use bytes::{Buf, BufMut, Bytes, BytesMut};
use rand::{rngs::StdRng, SeedableRng};
use std::{
    io::{Error, ErrorKind},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex,
    time::{sleep, Duration},
};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("0.0.0.0:25565").await?;
    println!("Server listening on port 25565");

    loop {
        let (stream, _) = listener.accept().await?;
        let stream = Arc::new(Mutex::new(stream));

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                eprintln!("Connection error: {}", e);
            }
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub enum State {
    HandShake = 0,
    Status = 1,
    Login = 2,
    Transfer = 3,
    Play = 4,
    Config = 5,
}

#[derive(Debug)]
struct KeepAliveState {
    current_keep_alive_id: Mutex<u64>,
    last_response_time: Mutex<std::time::Instant>,
    rng: Mutex<StdRng>,
}

impl KeepAliveState {
    pub fn new() -> Self {
        Self {
            current_keep_alive_id: Mutex::new(0),
            last_response_time: Mutex::new(std::time::Instant::now()),
            rng: Mutex::new(StdRng::from_entropy()),
        }
    }

    pub async fn get_current(&self) -> u64 {
        *self.current_keep_alive_id.lock().await
    }

    pub async fn set(&self, value: u64) {
        let mut id = self.current_keep_alive_id.lock().await;
        *id = value;
    }

    pub async fn update_last_response(&self) {
        let mut time = self.last_response_time.lock().await;
        *time = std::time::Instant::now();
    }

    pub async fn is_connection_alive(&self) -> bool {
        let time = self.last_response_time.lock().await;
        time.elapsed() < Duration::from_secs(30)
    }

    pub async fn generate_id(&self) -> u64 {
        use rand::Rng;
        let mut rng = self.rng.lock().await;
        rng.gen()
    }
}

pub struct CurrentState {
    current_state: State,
}

impl CurrentState {
    pub fn new(initial_state: State) -> Self {
        Self {
            current_state: initial_state,
        }
    }

    pub async fn set_state(&mut self, new_state: State) {
        self.current_state = new_state;
    }

    pub async fn get_state(&self) -> State {
        self.current_state
    }
}

async fn handle_connection(stream: Arc<Mutex<TcpStream>>) -> Result<(), Error> {
    let _current_state = CurrentState::new(State::HandShake);

    handshake_state(Arc::clone(&stream)).await?;
    login_state(Arc::clone(&stream)).await?;
    config_state(Arc::clone(&stream)).await?;

    let keep_alive_id = Arc::new(KeepAliveState::new());
    play_state(stream, keep_alive_id).await?;

    Ok(())
}

async fn play_state(
    stream: Arc<Mutex<TcpStream>>,
    keep_alive_id: Arc<KeepAliveState>,
) -> Result<(), Error> {
    // Spawn keep-alive sender task
    let stream_clone = Arc::clone(&stream);
    let keep_alive_clone = Arc::clone(&keep_alive_id);

    let keep_alive_handle =
        tokio::spawn(async move { send_keep_alive(stream_clone, keep_alive_clone).await });

    // Spawn connection monitor
    let keep_alive_clone = Arc::clone(&keep_alive_id);
    let monitor_handle = tokio::spawn(async move { monitor_connection(keep_alive_clone).await });

    let result = async {
        loop {
            let ready = stream.lock().await.ready(Interest::READABLE).await?;
            if ready.is_readable() {
                let (prot, mut buf) = handle_packet(Arc::clone(&stream)).await?;
                match prot {
                    0x1A => {
                        println!("Client sent keep alive");
                        handle_keep_alive(&mut buf, &keep_alive_id).await?;
                    }
                    _ => println!("Protocol not recognized: 0x{:X}", prot),
                }
            } else {
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
    .await;

    // Cancel background tasks when main loop exits
    keep_alive_handle.abort();
    monitor_handle.abort();

    result
}

async fn monitor_connection(keep_alive_state: Arc<KeepAliveState>) {
    loop {
        sleep(Duration::from_secs(5)).await;

        if !keep_alive_state.is_connection_alive().await {
            eprintln!("Connection timed out - no keep-alive response received in 30 seconds");
            break;
        }
    }
}

async fn handle_keep_alive(
    buf: &mut Bytes,
    keep_alive_state: &KeepAliveState,
) -> Result<(), Error> {
    let client_id = buf.get_u64();
    let server_id = keep_alive_state.get_current().await;

    if client_id != server_id {
        return Err(Error::new(ErrorKind::InvalidData, "Keep-alive ID mismatch"));
    }

    keep_alive_state.update_last_response().await;

    Ok(())
}

async fn send_keep_alive(stream: Arc<Mutex<TcpStream>>, keep_alive_id: Arc<KeepAliveState>) {
    println!("Starting keep-alive task");

    loop {
        sleep(Duration::from_secs(15)).await;

        let mut buf = BytesMut::new();
        let id = keep_alive_id.generate_id().await;

        keep_alive_id.set(id).await;
        buf.put_u64(id);

        if let Err(e) = send_packet(Arc::clone(&stream), 0x27, buf.freeze()).await {
            eprintln!("Failed to send keep-alive: {}", e);
            break;
        }
        println!("Sent keep-alive with ID: {}", id);
    }
}

async fn handshake_state(stream: Arc<Mutex<TcpStream>>) -> Result<State, Error> {
    let (_, mut buf) = handle_packet(stream).await?;
    let handshake_packet = handle_handshake(&mut buf)?;

    Ok(handshake_packet.next_state)
}

async fn login_state(stream: Arc<Mutex<TcpStream>>) -> Result<State, Error> {
    let (_, mut buf) = handle_packet(Arc::clone(&stream)).await?;
    let (name, uuid) = handle_login_client(&mut buf)?;

    send_login_success(stream, name, uuid).await?;
    Ok(State::Config)
}

async fn config_state(stream: Arc<Mutex<TcpStream>>) -> Result<State, Error> {
    send_packet(Arc::clone(&stream), 3, Bytes::new()).await?;
    handle_packet(stream).await?; // Wait for config ack
    Ok(State::Play)
}

async fn send_login_success(
    stream: Arc<Mutex<TcpStream>>,
    name: String,
    uuid: Uuid,
) -> Result<(), Error> {
    let mut buf = BytesMut::new();
    let (uuid_high, uuid_low) = uuid.as_u64_pair();
    buf.put_u64(uuid_high);
    buf.put_u64(uuid_low);

    write_var_int(name.len() as u32, &mut buf)?;
    buf.put_slice(name.as_bytes());

    write_var_int(0, &mut buf)?; // Empty properties array

    send_packet(stream, 0x02, buf.freeze()).await
}

fn write_var_int(value: u32, buf: &mut BytesMut) -> Result<u8, Error> {
    const SEGMENT_BITS: u32 = 0x7F;
    const CONTINUE_BIT: u32 = 0x80;
    let mut val = value;
    let mut size = 0;
    loop {
        if (val & !SEGMENT_BITS) == 0 {
            buf.put_u8((val & SEGMENT_BITS) as u8);
            size += 1;
            break;
        }
        buf.put_u8(((val & SEGMENT_BITS) | CONTINUE_BIT) as u8);
        size += 1;
        val >>= 7;
    }
    Ok(size)
}

async fn send_packet(
    stream: Arc<Mutex<TcpStream>>,
    protocol: u32,
    buf: Bytes,
) -> Result<(), Error> {
    println!("Starting to send packet with protocol: 0x{:X}", protocol);
    let mut packet_buf = BytesMut::new();
    let mut prot_buf = BytesMut::new();

    let prot_len = write_var_int(protocol, &mut prot_buf)?;
    write_var_int((buf.len() + prot_len as usize) as u32, &mut packet_buf)?;

    let mut combined = BytesMut::with_capacity(packet_buf.len() + prot_buf.len() + buf.len());
    combined.extend_from_slice(&packet_buf);
    combined.extend_from_slice(&prot_buf);
    combined.extend_from_slice(&buf);

    println!("About to acquire stream lock writing");
    let mut stream = stream.lock().await;
    println!("Lock acquired, writing {} bytes writing", combined.len());
    stream.write_all(&combined).await?;
    println!("Write completed");
    Ok(())
}

async fn handle_packet(stream: Arc<Mutex<TcpStream>>) -> Result<(u32, Bytes), Error> {
    println!("About to acquire stream lock reading");
    let mut stream = stream.lock().await;
    println!("Lock acquired reading");
    let packet_length = read_packet_length(&mut stream).await?;
    println!("packet length {}", packet_length);

    let mut buffer = BytesMut::with_capacity(packet_length as usize);
    buffer.resize(packet_length as usize, 0);
    stream.read_exact(&mut buffer).await?;

    let mut bytes = buffer.freeze();
    let packet_id = read_var_int(&mut bytes)?;
    println!("Read completed");
    Ok((packet_id, bytes))
}

fn read_var_int(buf: &mut Bytes) -> Result<u32, Error> {
    const SEGMENT_BITS: u8 = 0x7F;
    const CONTINUE_BIT: u8 = 0x80;
    let mut value: u32 = 0;
    let mut position = 0;

    loop {
        if buf.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Buffer empty while reading VarInt",
            ));
        }

        let current_byte = buf[0];
        buf.advance(1); // Remove the byte we just read

        value |= ((current_byte & SEGMENT_BITS) as u32) << position;

        if (current_byte & CONTINUE_BIT) == 0 {
            break;
        }

        position += 7;
        if position >= 32 {
            return Err(Error::new(ErrorKind::InvalidData, "VarInt is too big"));
        }
    }
    Ok(value)
}

async fn read_packet_length(stream: &mut TcpStream) -> Result<u32, Error> {
    let mut value = 0u32;
    let mut position = 0;
    let mut byte = [0u8; 1];

    loop {
        stream.read_exact(&mut byte).await?;
        value |= u32::from(byte[0] & 0x7F) << position;

        if (byte[0] & 0x80) == 0 {
            return Ok(value);
        }

        position += 7;
        if position >= 32 {
            return Err(Error::new(ErrorKind::InvalidData, "VarInt too big"));
        }
    }
}

// Helper functions (same as before but with async removed where not needed)

fn read_unsigned_short(buf: &mut Bytes) -> Result<u16, Error> {
    if buf.len() < 2 {
        return Err(Error::new(ErrorKind::UnexpectedEof, "buffer too short"));
    }
    let bytes = buf.split_to(2);
    Ok(u16::from_be_bytes(bytes[..].try_into().unwrap()))
}

struct HandshakePacket {
    protocol_version: u32,
    server_address: String,
    server_port: u16,
    next_state: State,
}

fn handle_handshake(buf: &mut Bytes) -> Result<HandshakePacket, Error> {
    let protocol_version = read_var_int(buf)?;
    let server_address = read_string(buf)?;
    let server_port = read_unsigned_short(buf)?;
    let next_state_value = read_var_int(buf)?;

    let next_state = match next_state_value {
        1 => State::Status,
        2 => State::Login,
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("Invalid state value: {}", next_state_value),
            ))
        }
    };

    Ok(HandshakePacket {
        protocol_version,
        server_address,
        server_port,
        next_state,
    })
}

fn handle_login_client(buf: &mut Bytes) -> Result<(String, Uuid), Error> {
    let name = read_string(buf)?;
    let uuid = read_uuid(buf)?;
    Ok((name, uuid))
}

fn read_string(buf: &mut Bytes) -> Result<String, Error> {
    let len = read_var_int(buf)? as usize;
    if buf.len() < len {
        return Err(Error::new(ErrorKind::UnexpectedEof, "Buffer too short"));
    }
    Ok(String::from_utf8_lossy(&buf.split_to(len)).into_owned())
}

fn read_uuid(buf: &mut Bytes) -> Result<Uuid, Error> {
    let bytes: [u8; 16] = buf
        .get(..16)
        .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "Incomplete UUID"))?
        .try_into()
        .unwrap();
    buf.advance(16);
    Ok(Uuid::from_bytes(bytes))
}

// VarInt/VarLong functions remain the same as original
// [Include your existing read_var_int, write_var_int, etc. implementations here]
