#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::env;
use std::error;
use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::{Buf, BufMut, BytesMut};
use enumflags2::BitFlags;
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

struct EventTable<'a> {
    table: HashMap<u32, Box<dyn Fn(&mut dyn Buf, u16, u16) + Send + 'a>>,
}

impl<'a> EventTable<'a> {
    fn new() -> Self {
        Self {
            table: HashMap::new(),
        }
    }

    fn insert<F: 'a + Fn(&mut dyn Buf, u16, u16) + Send>(&mut self, id: u32, f: F) {
        self.table.insert(id, Box::new(f));
    }

    fn decode(&self, sender: u32, buf: &mut dyn Buf, opcode: u16, length: u16) -> bool {
        if self.table.contains_key(&sender) {
            self.table[&sender](buf, opcode, length);
            true
        } else {
            false
        }
    }
}

fn decode_wl_string(buf: &mut dyn Buf) -> String {
    let len = buf.get_u32_le() as usize;
    let wl_string = CStr::from_bytes_with_nul(&buf.chunk()[..len])
        .unwrap()
        .to_string_lossy()
        .into_owned();
    buf.advance(len + pad(len));

    wl_string
}

mod wl {
    use enumflags2::bitflags;

    #[bitflags]
    #[repr(u32)]
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum SeatCapability {
        Pointer = 1,
        Keyboard = 2,
        Touch = 4,
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

    let object_cache = Arc::new(std::sync::Mutex::new(ObjectCache::new()));
    let event_table = Arc::new(Mutex::new(EventTable::new()));

    let (event_barrier_tx, mut event_barrier_rx): (
        mpsc::Sender<(u32, oneshot::Sender<_>)>,
        mpsc::Receiver<(u32, oneshot::Sender<_>)>,
    ) = mpsc::channel(1);
    let object_cache_for_task = object_cache.clone();
    let event_table_for_task = event_table.clone();
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

                // make sure we can read the rest of the event in one go
                while response.remaining() < (length as usize - 8) {
                    read_stream.read_buf(&mut response).await?;
                }

                eprintln!(
                    "sender = {}, length = {}, opcode = {}",
                    sender, length, opcode,
                );

                {
                    let event_table_inner = event_table_for_task.lock().await;
                    if event_table_inner.decode(sender, &mut response, opcode, length) {
                        continue;
                    }
                }

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
                } else {
                    // unknown event: we ignore unknown event
                    response.advance(length as usize - 8);
                }
            }
            response = response.split();
        } // loop

        Ok::<(), std::io::Error>(())
    });

    let message_size = 12u16;
    let mut connection_req = BytesMut::with_capacity(message_size.into());
    let wl_display_id = id_generator.next().unwrap();
    {
        let mut event_table_inner = event_table.lock().await;
        event_table_inner.insert(
            wl_display_id,
            move |response: &mut dyn Buf, opcode: u16, length: u16| {
                if opcode == 0 {
                    // wl_display::error
                    let object_id = response.get_u32_le();
                    let code = response.get_u32_le();
                    let message = decode_wl_string(response);
                    eprintln!(
                        "wl_display::error {}, object_id = {}, code = {}",
                        message, object_id, code
                    );
                } else if opcode == 1 {
                    // wl_display::delete_id
                    eprintln!("wl_display::delete_id {}", response.get_u32_le());
                }
            },
        );
    }
    connection_req.put_u32_le(wl_display_id);
    connection_req.put_u16_le(1); // get_registry
    connection_req.put_u16_le(message_size);
    let wl_registry_id = id_generator.next().unwrap();
    connection_req.put_u32_le(wl_registry_id); // id for the new wl_registry - global registry object
    {
        let mut event_table_inner = event_table.lock().await;
        event_table_inner.insert(
            wl_registry_id,
            move |response: &mut dyn Buf, opcode: u16, length: u16| {
                // wl_registry::global
                let name = response.get_u32_le();
                let interface = decode_wl_string(response);

                let version = response.get_u32_le();
                eprintln!(
                    "  name = {}, interface = {:?}, version = {}",
                    name, interface, version,
                );

                {
                    let o = Object {
                        name,
                        interface,
                        version,
                    };

                    let mut inner_cache = object_cache_for_task.lock().unwrap();

                    inner_cache.insert(o);
                }
            },
        );
    }
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
        let inner_cache = object_cache.lock().unwrap();
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
        let inner_cache = object_cache.lock().unwrap();
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
        let inner_cache = object_cache.lock().unwrap();
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
    {
        let mut event_table_inner = event_table.lock().await;
        event_table_inner.insert(
            wl_seat_id,
            move |response: &mut dyn Buf, opcode: u16, length: u16| {
                if opcode == 0 {
                    // wl_seat::capabilities
                    let capabilities = response.get_u32_le();
                    let capabilities: BitFlags<wl::SeatCapability> =
                        BitFlags::from_bits(capabilities).expect("valid wl_seat capabilities");
                    eprintln!("capability = {:#?}", capabilities);
                } else if opcode == 1 {
                    let seat = decode_wl_string(response);
                    eprintln!("seat = {}", seat);
                }
            },
        );
    }
    write_stream.write_all_buf(&mut connection_req).await?;

    //todo!("handle wl_seat::capabilities and wl_seat::name");

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
    }

    // get_pointer
    connection_req.clear();
    connection_req.put_u32_le(wl_seat_id);
    connection_req.put_u16_le(0); // wl_seat::get_pointer
    connection_req.put_u16_le(12); // request length
    let wl_pointer_id = id_generator.next().unwrap();
    connection_req.put_u32_le(dbg!(wl_pointer_id));

    // get_keyboard
    connection_req.put_u32_le(wl_seat_id);
    connection_req.put_u16_le(1); // wl_seat::get_keyboard
    connection_req.put_u16_le(12); // request length
    let wl_keyboard_id = id_generator.next().unwrap();
    connection_req.put_u32_le(dbg!(wl_keyboard_id));
    {
        let mut event_table_inner = event_table.lock().await;
        event_table_inner.insert(
            wl_keyboard_id,
            move |response: &mut dyn Buf, opcode: u16, length: u16| {
                if opcode == 0 {
                    // wl_keyboard::keymap
                    //
                    // The format of the message is unclear. The
                    // message header says length 16, which means 8
                    // bytes for header and 8 bytes for payload. And
                    // according to the documentation, the payload
                    // should contain 4 bytes for "format", 4 bytes
                    // for "fd" and 4 bytes for "size", which is too
                    // much. How can that be?
                    //
                    // The second parameter "fd" of type "fd" is
                    // somehow passed via "ancillary data" of the Unix
                    // domain socket. From the Wayland documentation
                    // "The file descriptor is not stored in the
                    // message buffer, but in the ancillary data of
                    // the UNIX domain socket message
                    // (msg_control).". WTF is "ancillary data"?
                    let format = response.get_u32_le();
                    let fd = 0;
                    let keymap_size_bytes = response.get_u32_le();
                    eprintln!(
                        "format = {}, fd = {}, keymap_size_bytes = {}",
                        format, fd, keymap_size_bytes
                    );
                } else if opcode == 5 {
                    // wl_keyboard::repeat_info
                    let rate = response.get_i32_le(); // characters per second
                    let delay = response.get_i32_le(); // in milliseconds
                    eprintln!("rate = {}, delay = {}", rate, delay);
                }
            },
        );
    }

    // write a combined get_pointer & get_keyboard request
    write_stream.write_all_buf(&mut connection_req).await?;

    handle.await??;

    Ok(())
}
