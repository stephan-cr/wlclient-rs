#![warn(rust_2018_idioms)]

use std::env;
use std::error;
use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    oneshot, Mutex,
};

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

const fn pad(len: usize) -> usize {
    (4 - (len % 4)) % 4
}

struct Object {
    name: u32,
    interface: String,
    version: u32,
}

struct ObjectCache {
    cache: Vec<Object>,
}

impl ObjectCache {
    fn new() -> Self {
        Self { cache: vec![] }
    }

    fn insert(&mut self, o: Object) {
        self.cache.push(o);
    }

    fn lookup_by_interface_name(&self, interface_name: &str) -> Option<&Object> {
        self.cache.iter().find(|&x| x.interface == interface_name)
    }

    fn lookup_by_name(&self, name: u32) -> Option<&Object> {
        self.cache.iter().find(|&x| x.name == name)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn error::Error>> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let display = env::var("WAYLAND_DISPLAY")?;

    let mut path_to_socket = PathBuf::from(xdg_runtime_dir);
    path_to_socket.push(display);

    let mut id_generator = IdGenerator::new();

    let stream = UnixStream::connect(&path_to_socket).await?;
    let (mut read_stream, mut write_stream) = stream.into_split();

    let object_cache = Arc::new(Mutex::new(ObjectCache::new()));

    let (event_barrier_tx, mut event_barrier_rx): (
        mpsc::Sender<(u32, oneshot::Sender<_>)>,
        mpsc::Receiver<(u32, oneshot::Sender<_>)>,
    ) = mpsc::channel(1);
    let object_cache_for_task = object_cache.clone();
    let handle = tokio::spawn(async move {
        let mut response = BytesMut::new();
        loop {
            let n = read_stream.read_buf(&mut response).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Wayland server gave up on us",
                ));
            }

            while response.remaining() >= 8
            /* make sure we're able to read the event header */
            {
                let sender = response.get_u32_le();
                let opcode = response.get_u16_le();
                let length = response.get_u16_le();

                eprintln!(
                    "sender = {}, length = {}, opcode = {}",
                    sender, length, opcode,
                );

                if sender == 2 && opcode == 0 {
                    while response.remaining() < 8 {
                        read_stream.read_buf(&mut response).await?;
                    }
                    let name = response.get_u32_le();
                    let len = response.get_u32_le() as usize;
                    while response.remaining() < len + pad(len) + 4 {
                        read_stream.read_buf(&mut response).await?;
                    }
                    let interface = CStr::from_bytes_with_nul(&response[..len])
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    response.advance(len + pad(len));

                    let version = response.get_u32_le();
                    eprintln!(
                        "  name = {}, interface = {:?}, version = {}",
                        name, interface, version,
                    );

                    {
                        let o = Object {
                            name: name,
                            interface: interface,
                            version: version,
                        };

                        let mut inner_cache = object_cache_for_task.lock().await;

                        inner_cache.insert(o);
                    }
                } else if sender == 1 && opcode == 0 {
                    // wl_display::error
                    let object_id = response.get_u32_le();
                    let code = response.get_u32_le();
                    let len = response.get_u32_le() as usize;
                    while response.remaining() < len + pad(len) {
                        read_stream.read_buf(&mut response).await?;
                    }
                    let message = CStr::from_bytes_with_nul(&response[..len])
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    response.advance(len + pad(len));
                    eprintln!(
                        "wl_display::error {}, object_id = {}, code = {}",
                        message, object_id, code
                    );
                } else if sender == 1 && opcode == 1 {
                    // wl_display::delete_id
                    eprintln!("wl_display::delete_id {}", response.get_u32_le());
                } else {
                    let event_barrier = match event_barrier_rx.try_recv() {
                        Ok((sender, back_channel)) => Some((sender, back_channel)),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "All senders disconnected",
                            ))
                        }
                    };

                    if event_barrier.is_some() && opcode == 0 {
                        if let Some((_sender, back_channel)) = event_barrier {
                            // todo!("the sender #3 is an hard coded value, but each for callback a new sender id is generated");
                            // let back_channel = event_barrier_rx.recv().await.unwrap();
                            back_channel.send(response.get_u32_le()).unwrap();
                        }
                    }
                }
            }
            response = response.split();
        } // loop

        Ok::<(), std::io::Error>(())
    });

    let message_size = 12u16;
    let mut connection_req = BytesMut::with_capacity(message_size.into());
    let wl_display_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_display_id);
    connection_req.put_u16_le(1); // get_registry
    connection_req.put_u16_le(message_size);
    let wl_registry_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_registry_id); // id for the new wl_registry - global registry object
    write_stream.write_all_buf(&mut connection_req).await?;

    // make sure the object cache contains all current objects
    {
        connection_req.put_u32_le(wl_display_id);
        connection_req.put_u16_le(0); // sync
        connection_req.put_u16_le(message_size);
        let wl_callback_id = id_generator.next().unwrap();
        connection_req.put_u32_le(wl_callback_id); // id for the new wl_callback - callback object for the sync request
        let (one_tx, one_rx) = oneshot::channel();
        event_barrier_tx.send((wl_callback_id, one_tx)).await?;
        write_stream.write_all_buf(&mut connection_req).await?;
        one_rx.await.unwrap();
        eprintln!("global object cache populated");
    }

    let (wl_compositor_name, wl_compositor_version) = {
        let inner_cache = object_cache.lock().await;
        inner_cache
            .lookup_by_interface_name("wl_compositor")
            .map(|object| (object.name, object.version))
    }
    .unwrap();

    // bind wl_compositor
    connection_req.clear();
    connection_req.put_u32_le(wl_registry_id);
    connection_req.put_u16_le(0); // wl_registry::bind
    connection_req.put_u16_le(40); // request length
    connection_req.put_u32_le(wl_compositor_name); // interface "name"
    connection_req.put_u32_le(14); // length of next string, including \0
    connection_req.put_slice(&b"wl_compositor\0  "[..]);
    connection_req.put_u32_le(wl_compositor_version); // wl_compositor version
    let wl_compositor_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_compositor_id);
    write_stream.write_all_buf(&mut connection_req).await?;

    connection_req.clear();
    connection_req.put_u32_le(wl_compositor_id);
    connection_req.put_u16_le(0); // wl_compositor::create_surface
    connection_req.put_u16_le(12); // request length
    let wl_surface_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_surface_id);
    write_stream.write_all_buf(&mut connection_req).await?;

    let (xdg_wm_base_name, xdg_wm_base_version) = {
        let inner_cache = object_cache.lock().await;
        inner_cache
            .lookup_by_interface_name("xdg_wm_base")
            .map(|object| (object.name, object.version))
    }
    .unwrap();

    {
        // checkout why this doesn't work

        // bind xdg_wm_base
        // connection_req.clear();
        // connection_req.put_u32_le(wl_registry_id);
        // connection_req.put_u16_le(0); // wl_registry::bind
        // connection_req.put_u16_le(36); // request length
        // connection_req.put_u32_le(xdg_wm_base_name); // interface "name"
        // connection_req.put_u32_le(12); // length of next string, including \0
        // connection_req.put_slice(&b"xdg_wm_base\0"[..]);
        // connection_req.put_u32_le(xdg_wm_base_version);
        // let xdg_wm_base_id = id_generator.next().unwrap();
        // connection_req.put_u32_le(xdg_wm_base_id);
        // write_stream.write_all_buf(&mut connection_req).await?;

        // // create an xdg_surface
        // connection_req.clear();
        // connection_req.put_u32_le(xdg_wm_base_id);
        // connection_req.put_u16_le(3); // xdg_wm_base::get_xdg_surface
        // connection_req.put_u16_le(16); // request length
        // let xdg_surface_id = id_generator.next().unwrap();
        // connection_req.put_u32_le(xdg_surface_id);
        // connection_req.put_u32_le(wl_surface_id);
        // write_stream.write_all_buf(&mut connection_req).await?;

        // eprintln!("xdg_surface_id {}, xdg_wm_base_id {}", xdg_surface_id, xdg_wm_base_id);
    }

    let (wl_seat_name, wl_seat_version) = {
        let inner_cache = object_cache.lock().await;
        inner_cache
            .lookup_by_interface_name("wl_seat")
            .map(|object| (object.name, object.version))
    }
    .unwrap();

    // bind wl_seat
    connection_req.clear();
    connection_req.put_u32_le(wl_registry_id);
    connection_req.put_u16_le(0); // wl_registry::bind
    connection_req.put_u16_le(32); // request length
    connection_req.put_u32_le(wl_seat_name); // interface "name"
    connection_req.put_u32_le(8); // length of next string, including \0
    connection_req.put_slice(&b"wl_seat\0"[..]);
    connection_req.put_u32_le(wl_seat_version);
    let wl_seat_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_seat_id);
    eprintln!(
        "connection_req = {:?}, {}",
        &connection_req, wl_seat_version
    );
    write_stream.write_all_buf(&mut connection_req).await?;

    todo!("handle wl_seat::capabilities and wl_seat::name");

    // make sure the object cache contains all current objects
    {
        connection_req.clear();
        connection_req.put_u32_le(wl_display_id);
        connection_req.put_u16_le(0); // sync
        connection_req.put_u16_le(message_size);
        let wl_callback_id = id_generator.next().unwrap();
        connection_req.put_u32_le(wl_callback_id); // id for the new wl_callback - callback object for the sync request
        let (one_tx, one_rx) = oneshot::channel();
        event_barrier_tx.send((wl_callback_id, one_tx)).await?;
        write_stream.write_all_buf(&mut connection_req).await?;
        one_rx.await.unwrap();
        eprintln!("global object cache populated");
    }

    handle.await??;

    Ok(())
}
