use bytes::{Buf, BufMut, Bytes, BytesMut};
use rand::Rng;
use std::{
    io::{Error, ErrorKind, Read, Write},
    net::TcpListener,
    sync::Arc,
};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("0.0.0.0:25565").unwrap();
    for stream in listener.incoming() {
        handle_connection(&mut stream.unwrap());
    }
}

pub enum State {
    HandShake = 0,
    Status = 1,
    Login = 2,
    Transfer = 3,
    Play = 4,
    Config = 5,
}

struct KeepAliveState {
    current_keep_alive_id: Mutex<u64>,
}

impl KeepAliveState {
    pub fn new() -> Self {
        Self {
            current_keep_alive_id: Mutex::new(0),
        }
    }

    pub async fn get_current(&self) -> u64 {
        *self.current_keep_alive_id.lock().await
    }

    pub async fn set(&self, value: u64) {
        let mut id = self.current_keep_alive_id.lock().await;
        *id = value;
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
    pub fn set_state(&mut self, new_state: State) {
        self.current_state = new_state;
    }

    // Get the current state
    pub fn get_state(&self) -> &State {
        &self.current_state
    }
}

fn handle_connection(stream: &mut TcpStream) {
    let _current_state = CurrentState::new(State::HandShake);
    handshake_state(stream);
    login_state(stream);
    config_state(stream);
    let keep_alive_id = KeepAliveState::new();
    play_state(stream, keep_alive_id);
}

fn play_state(
    stream: Arc<Mutex<TcpStream>>,
    keep_alive_id: KeepAliveState,
) -> Result<State, Error> {
    send_keep_alive(stream, keep_alive_id);
    loop {
        let res = handle_packet(stream)?;
        let prot = res.0;
        let mut buf = res.1;
        match prot {
            0x1A => {
                println!("Client send keep alive");
                // Handle keep-alive packet
                if let Err(e) = handle_keep_alive(&mut buf) {
                    // Handle the error (e.g., log it or return it)
                    return Err(e);
                }
            }
            _ => {
                println!("Protocol not recognized");
                // Optionally, handle unrecognized protocols
            }
        }
    }
}

//________________Handle Play Packages_________________

//TODO finish this make sure it actually matches
fn handle_keep_alive(buf: &mut Bytes) -> Result<(), Error> {
    let _client_keep_alive_id = buf.get_u64();
    Ok(())
}

//_____________________________________________________

async fn send_keep_alive(stream: &mut TcpStream, keep_alive_id: KeepAliveState) {
    let mut rng = rand::thread_rng();
    println!("I started keep alive");
    loop {
        sleep(Duration::from_secs(15)).await;
        let mut buf = BytesMut::new();

        let keep_alive_id_ran: u64 = rng.gen();
        keep_alive_id.set(keep_alive_id_ran).await;
        buf.put_u64(keep_alive_id_ran);
        let _res = send_packet(stream, 0x27, buf.freeze());
        println!("I sent keep alive");
        //Todo handle error
    }
}

fn handshake_state(stream: &mut TcpStream) -> Result<State, Error> {
    let err = handle_packet(stream)?;
    let _prot = err.0;
    let mut buf = err.1;

    let hand_shake_packet = handle_handshake(&mut buf)?;

    Ok(hand_shake_packet.next_state)
}

//fn status_state(stream: &mut TcpStream) -> Result<State, Error> {}

fn login_state(stream: &mut TcpStream) -> Result<State, Error> {
    let mut buf = handle_packet(stream)?.1;
    let player_name_uuid = handle_login_client(&mut buf)?;
    let _ = send_login_success(stream, player_name_uuid.0, player_name_uuid.1);
    Ok(State::Config)
}

//fn transfer_state(stream: &mut TcpStream) -> Result<State, Error> {}

fn config_state(stream: &mut TcpStream) -> Result<State, Error> {
    let buf = Bytes::new();
    send_packet(stream, 3, buf)?;
    let _finish_config_ack = handle_packet(stream).unwrap();
    Ok(State::Play)
}

fn send_login_success(stream: &mut TcpStream, name: String, uuid: Uuid) -> Result<(), Error> {
    let mut buf = BytesMut::new();
    buf.put_u64(uuid.as_u64_pair().0);
    buf.put_u64(uuid.as_u64_pair().1);
    //println!("Uuid: {}", hex::encode(buf.clone()));

    let _len_name = write_var_int(name.as_bytes().len() as u32, &mut buf)?;
    //println!("Uuid + str len: {}", hex::encode(buf.clone()));
    let name_bytes = name.as_bytes();
    buf.put_slice(name_bytes);

    // TODO write actual properties array
    let _len_array = write_var_int(0, &mut buf)?;
    // println!("Uuid + str len + name: {}", hex::encode(buf.clone()));
    send_packet(stream, 0x02, buf.freeze())?;
    Ok(())
}

fn send_packet(stream: &mut TcpStream, protocol: u32, buf: Bytes) -> Result<(), Error> {
    let mut packet_buf = BytesMut::new(); // Buffer for packet length
    let mut prot_buf = BytesMut::new(); // Buffer for protocol ID

    // Write the protocol ID as a variable-length integer
    let prot_len = write_var_int(protocol, &mut prot_buf)?;
    let data_len = buf.len();
    let pack_len = data_len + prot_len as usize;
    // Write the packet length as a variable-length integer
    write_var_int(pack_len as u32, &mut packet_buf)?;

    // Combine all buffers: packet length, protocol ID, and data
    let mut combined = BytesMut::with_capacity(packet_buf.len() + prot_buf.len() + buf.len());
    combined.extend_from_slice(&packet_buf);
    combined.extend_from_slice(&prot_buf);
    combined.extend_from_slice(&buf);
    println!("Combined package (hex): {}", hex::encode(&combined));
    // Write the combined buffer to the TCP stream
    stream.write_all(&combined)?;

    Ok(())
}

fn handle_packet(stream: &mut TcpStream) -> Result<(u32, Bytes), Error> {
    let packet_length = read_packet_length(stream)?;

    let mut bufmut = BytesMut::with_capacity(packet_length as usize);

    // Resize the buffer so it can hold the required amount of data
    bufmut.resize(packet_length as usize, 0);

    // Read data from the stream into the BytesMut buffer
    stream.read_exact(&mut bufmut)?;
    let mut buf = bufmut.freeze();
    let packet_id = read_var_int(&mut buf)?;
    Ok((packet_id, buf))
}

fn handle_login_client(buf: &mut Bytes) -> Result<(String, Uuid), Error> {
    let player_name = read_string(buf)?;
    let player_uuid = read_uuid(buf)?;
    println!("Player Name {}", player_name);
    println!("Player Uuid {}", player_uuid);
    Ok((player_name, player_uuid))
    // Send Login Success with empty properties array. TODO populate properties array. Think this
    // is used to send player skin and cape?
}

fn read_uuid(buf: &mut Bytes) -> Result<Uuid, Error> {
    let uuid_bytes = buf.split_to(16);
    let mut uuid_array = [0u8; 16];
    uuid_array.copy_from_slice(&uuid_bytes[..16]);
    let uuid = Uuid::from_bytes(uuid_array);
    Ok(uuid)
}

struct Player {
    player_name: String,
    player_id: Uuid,
}

fn read_packet_length(stream: &mut TcpStream) -> Result<u32, Error> {
    const SEGMENT_BITS: u8 = 0x7F;
    const CONTINUE_BIT: u8 = 0x80;
    let mut value: u32 = 0;
    let mut position = 0;
    let mut buf = [0; 1];

    loop {
        if let Err(_e) = stream.read_exact(&mut buf) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Buffer empty while reading VarInt",
            ));
        }

        let current_byte = buf[0];

        value |= ((current_byte & SEGMENT_BITS) as u32) << position;

        if (current_byte & CONTINUE_BIT) == 0 {
            break;
        }

        position += 7;
        if position >= 32 {
            return Err(Error::new(ErrorKind::InvalidData, "VarInt is too big"));
        }
    }
    return Ok(value);
}

pub struct HandshakePacket {
    pub protocol_version: u32,
    pub server_address: String,
    pub server_port: u16,
    pub next_state: State,
}

fn read_string(buf: &mut Bytes) -> Result<String, Error> {
    let len = read_var_int(buf)? as usize;
    println!("String len {}", len);
    if buf.len() < len {
        return Err(Error::new(ErrorKind::UnexpectedEof, "buffer too short"));
    }
    let str_bytes = buf.split_to(len);
    let str = String::from_utf8(str_bytes.to_vec()).expect("failed to convert bytes to string");
    Ok(str)
}

fn read_unsigned_short(buf: &mut Bytes) -> Result<u16, Error> {
    if buf.len() < 2 {
        return Err(Error::new(ErrorKind::UnexpectedEof, "buffer too short"));
    }
    let bytes = buf.split_to(2);
    Ok(u16::from_be_bytes(bytes[..].try_into().unwrap()))
}

fn handle_handshake(buf: &mut Bytes) -> Result<HandshakePacket, Error> {
    let protocol_version = read_var_int(buf)?;
    println!("Protocol Version {}", protocol_version);
    let server_address = read_string(buf)?;
    println!("Server Address {}", server_address);
    let server_port = read_unsigned_short(buf)?;
    println!("Server Port {}", server_port);
    let next_state_value = read_var_int(buf)?;
    println!("Next State Value {}", next_state_value);

    let next_state = match next_state_value {
        1 => State::Status,
        2 => State::Login,
        3 => State::Transfer,
        invalid => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid next state value: {}", invalid),
            ))
        }
    };

    let handshake_packet = HandshakePacket {
        protocol_version,
        server_address,
        server_port,
        next_state,
    };

    println!("test");

    Ok(handshake_packet)
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

fn read_var_long(buf: &mut Bytes) -> Result<u64, Error> {
    const SEGMENT_BITS: u8 = 0x7F;
    const CONTINUE_BIT: u8 = 0x80;
    let mut value: u64 = 0;
    let mut position = 0;

    loop {
        if buf.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Buffer empty while reading VarLong",
            ));
        }

        let current_byte = buf[0];
        buf.advance(1); // Remove the byte we just read

        value |= ((current_byte & SEGMENT_BITS) as u64) << position;

        if (current_byte & CONTINUE_BIT) == 0 {
            break;
        }

        position += 7;
        if position >= 64 {
            return Err(Error::new(ErrorKind::InvalidData, "VarLong is too big"));
        }
    }
    Ok(value)
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

fn write_var_long(value: u64, buf: &mut BytesMut) -> Result<(), Error> {
    const SEGMENT_BITS: u64 = 0x7F;
    const CONTINUE_BIT: u64 = 0x80;
    let mut val = value;

    loop {
        if (val & !SEGMENT_BITS) == 0 {
            buf.put_u8((val & SEGMENT_BITS) as u8);
            break;
        }
        buf.put_u8(((val & SEGMENT_BITS) | CONTINUE_BIT) as u8);
        val >>= 7;
    }
    Ok(())
}
