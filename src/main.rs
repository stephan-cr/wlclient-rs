use std::env;
use std::error;
use std::path::PathBuf;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const MAX_CLIENT_ID: u32 = 0xfe_ff_ff_ff;

// "Each object has a unique ID. The IDs are allocated by the entity
// creating the object (either client or server). IDs allocated by the
// client are in the range [1, 0xfeffffff] while IDs allocated by the
// server are in the range [0xff000000, 0xffffffff]. The 0 ID is
// reserved to represent a null or non-existant object."
// https://wayland.freedesktop.org/docs/html/ch04.html
struct IdGenerator {
    last_id: u32,
}

impl IdGenerator {
    fn new() -> Self {
        Self { last_id: 0 }
    }
}

impl Iterator for IdGenerator {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.last_id == MAX_CLIENT_ID {
            None
        } else {
            self.last_id += 1;
            Some(self.last_id)
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn error::Error>> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let display = env::var("WAYLAND_DISPLAY")?;

    let mut path_to_socket = PathBuf::from(xdg_runtime_dir);
    path_to_socket.push(display);

    let stream = UnixStream::connect(&path_to_socket).await?;
    let mut id_generator = IdGenerator::new();

    let (mut read_stream, mut write_stream) = stream.into_split();

    let handle = tokio::spawn(async move {
        let mut response = BytesMut::new();
        let n = read_stream.read_buf(&mut response).await?;
        eprintln!("{:?}", response);

        todo!("parse response");

        Ok::<(), std::io::Error>(())
    });

    let message_size = 12u16;
    let mut connection_req = BytesMut::with_capacity(message_size.into());
    connection_req.put_u32_le(id_generator.next().unwrap());
    connection_req.put_u16_le(1); // get_registry
    connection_req.put_u16_le(message_size);
    connection_req.put_u32_le(id_generator.next().unwrap()); // id for the new wl_registry - global registry object
    write_stream.write_all_buf(&mut connection_req).await?;

    handle.await??;

    Ok(())
}
