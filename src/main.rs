use std::{
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
};

use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::Loopback,
    socket::tcp,
    time::{Duration, Instant},
    wire::{EthernetAddress, IpAddress, IpCidr},
};
use tracing::Level;

// Adapted loopback example with a separate networking stack management type, that does polling internally.

fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(Level::TRACE)
            .finish(),
    )
    .unwrap();

    // Create sockets
    let server_socket = {
        // See smoltcp loopback example for commentary on this setup.
        static mut TCP_SERVER_RX_DATA: [u8; 4] = [0; 4];
        static mut TCP_SERVER_TX_DATA: [u8; 4] = [0; 4];
        let tcp_rx_buffer = tcp::SocketBuffer::new(unsafe { &mut TCP_SERVER_RX_DATA[..] });
        let tcp_tx_buffer = tcp::SocketBuffer::new(unsafe { &mut TCP_SERVER_TX_DATA[..] });
        tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer)
    };
    let client_socket = {
        static mut TCP_CLIENT_RX_DATA: [u8; 4] = [0; 4];
        static mut TCP_CLIENT_TX_DATA: [u8; 4] = [0; 4];
        let tcp_rx_buffer = tcp::SocketBuffer::new(unsafe { &mut TCP_CLIENT_RX_DATA[..] });
        let tcp_tx_buffer = tcp::SocketBuffer::new(unsafe { &mut TCP_CLIENT_TX_DATA[..] });
        tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer)
    };

    // Make a new networking stack object and register the sockets.
    let stack = Arc::new(DumbStack::new());
    let server_handle = stack.push_tcp_socket(server_socket);
    let client_handle = stack.push_tcp_socket(client_socket);

    // Okay, let's create a server thread and a client thread.
    let _stack = stack.clone();
    let server = std::thread::spawn(move || {
        // Step 1: Set the socket to listening.
        let mut did_listen = false;
        let stack = _stack;
        // This blocking function ensures that the networking stack internally makes progress while
        // we are waiting for some condition to become true. Here, we are waiting for .is_listening to return true.
        // It should do so immediately after we call .listen, but the rest of the code here is an example
        // of how we use this .blocking function when we do need to wait for something. This will be useful below.
        stack.blocking(|inner| {
            let sock = inner.get_socket_mut(server_handle);
            if !did_listen {
                sock.listen(1234).unwrap();
                did_listen = true;
            }
            // If our condition that we are waiting for is false, return None.
            if sock.is_listening() {
                Some(())
            } else {
                None
            }
        });
        tracing::info!("server socket is listening");

        // Wait until the socket becomes active (someone connects).
        stack.blocking(|inner| {
            let sock = inner.get_socket_mut(server_handle);
            if sock.is_active() {
                Some(())
            } else {
                None
            }
        });
        tracing::info!("server socket is active");

        // Read the data. Just print it for now.
        let mut buffer = [0; 1024];
        stack.blocking(|inner| {
            let sock = inner.get_socket_mut(server_handle);
            tracing::info!(
                "server reading: state = {:?}, {} {} {}",
                sock.state(),
                sock.is_active(),
                sock.is_open(),
                sock.can_recv()
            );

            // TODO: idk if there's a better wait to handle "remote closed the conn, so we need to as well".
            if sock.state() == tcp::State::CloseWait && !sock.can_recv() {
                sock.close();
                return Some(());
            }

            if !sock.is_active() {
                return Some(());
            }

            if sock.can_recv() {
                let len = sock.recv_slice(&mut buffer).unwrap();
                tracing::info!("got data: {:?}", &buffer[0..len]);
                None
            } else {
                None
            }
        });
        tracing::info!("server socket is done");
    });

    // Okay, on to the client.
    let _stack = stack.clone();
    let client = std::thread::spawn(move || {
        let mut did_conn = false;
        let stack = _stack;
        // Same idea as listen, above, but for the client, we are calling connect. That's implemented as a function in the DumbStack
        // because it needs context from the interface.
        stack.blocking(|inner| {
            if !did_conn {
                inner.connect(client_handle, IpAddress::v4(127, 0, 0, 1), 1234, 65000);
                did_conn = true;
            }
            let sock = inner.get_socket_mut(server_handle);
            if sock.is_open() {
                Some(())
            } else {
                None
            }
        });
        tracing::info!("client socket is connecting");

        // Wait for the connection to establish.
        stack.blocking(|inner| {
            let sock = inner.get_socket_mut(client_handle);
            if sock.is_active() {
                Some(())
            } else {
                None
            }
        });
        tracing::info!("client socket is active");

        // Send the data, one piece at a time. The buffer size above is set to pretty small, to test out having to chunk data.
        let send_buffer = b"hello world!";
        let mut pos = 0;
        stack.blocking(|inner| {
            if pos >= send_buffer.len() {
                tracing::info!("closing client socket");
                let sock = inner.get_socket_mut(client_handle);
                sock.close();
                return Some(());
            }
            let sock = inner.get_socket_mut(client_handle);
            if sock.can_send() {
                tracing::info!("sending {} bytes (pos = {})", send_buffer.len() - pos, pos);
                pos += sock.send_slice(&send_buffer[pos..]).unwrap();
                tracing::info!("   pos = {}", pos);
            }
            None
        });
        tracing::info!("client socket sent all data");
    });

    server.join().unwrap();
    client.join().unwrap();
}

// All the interface data and sockets.
struct DumbStackInner {
    interface: Interface,
    // Internally, this is owned. The docs say if we're using owned sockets here, we can use 'static for the lifetime bound.
    sockets: SocketSet<'static>,
    device: Loopback,
}

// Holds a reference to Inner, but also a background polling thread for the network state machines.
struct DumbStack {
    inner: Arc<Mutex<DumbStackInner>>,
    _thread: JoinHandle<()>,
    // Use this to activate the polling thread when state changes (eg a socket is added).
    channel: std::sync::mpsc::Sender<()>,
    // Allows threads to sleep while blocking.
    waiter: Arc<Condvar>,
}

impl DumbStackInner {
    fn new() -> Self {
        // This setup is adapted from the loopback example, and simplified.
        let mut device = Loopback::new(smoltcp::phy::Medium::Ethernet);
        let config = Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into());
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });

        Self {
            interface: iface,
            sockets: SocketSet::new(Vec::new()),
            device,
        }
    }

    fn push_tcp_socket(&mut self, socket: tcp::Socket<'static>) -> SocketHandle {
        self.sockets.add(socket)
    }

    fn get_socket_mut(&mut self, handle: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut(handle)
    }

    fn connect(&mut self, handle: SocketHandle, ip: IpAddress, dst_port: u16, local_port: u16) {
        let sock: &mut tcp::Socket = self.sockets.get_mut(handle);
        sock.connect(self.interface.context(), (ip, dst_port), local_port)
            .unwrap();
    }

    fn poll(&mut self, waiter: &Condvar) -> bool {
        let res = self
            .interface
            .poll(Instant::now(), &mut self.device, &mut self.sockets);
        // When we poll, notify the CV so that other waiting threads can retry their blocking operations.
        tracing::trace!("notify cv");
        waiter.notify_all();
        res
    }

    fn poll_time(&mut self) -> Option<Duration> {
        self.interface.poll_delay(Instant::now(), &mut self.sockets)
    }
}

impl DumbStack {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = Arc::new(Condvar::new());

        let inner = Arc::new(Mutex::new(DumbStackInner::new()));
        let _inner = inner.clone();
        let _waiter = waiter.clone();

        // Okay, here is our background polling thread. It polls the network interface with the SocketSet
        // whenever it needs to, which is:
        // 1. when smoltcp says to based on poll_time() (calls poll_delay internally)
        // 2. when the state changes (eg a new socket is added)
        // 3. when blocking threads need to poll (we get a message on the channel)
        let thread = std::thread::spawn(move || {
            let inner = _inner;
            let waiter = _waiter;
            loop {
                let time = {
                    let mut inner = inner.lock().unwrap();
                    let time = inner.poll_time();

                    // We may need to poll immediately!
                    if matches!(time, Some(Duration::ZERO)) {
                        tracing::trace!("poll thread polling");
                        inner.poll(&*waiter);
                        continue;
                    }
                    time
                };

                // Wait until the designated timeout, or until we get a message on the channel.
                match time {
                    Some(dur) => {
                        let _ = receiver.recv_timeout(dur.into());
                    }
                    None => {
                        receiver.recv().unwrap();
                    }
                }
            }
        });

        Self {
            inner,
            _thread: thread,
            channel: sender,
            waiter,
        }
    }

    fn push_tcp_socket(&self, socket: tcp::Socket<'static>) -> SocketHandle {
        let mut inner = self.inner.lock().unwrap();
        let handle = inner.push_tcp_socket(socket);
        self.channel.send(()).unwrap();
        handle
    }

    // Block until f returns Some(R), and then return R. Note that f may be called multiple times, and it may
    // be called spuriously.
    fn blocking<R>(&self, mut f: impl FnMut(&mut DumbStackInner) -> Option<R>) -> R {
        let mut inner = self.inner.lock().unwrap();
        tracing::trace!("polling from blocking");
        // Immediately poll, since we wait to have as up-to-date state as possible.
        inner.poll(&self.waiter);
        loop {
            // We'll need the polling thread to wake up and do work.
            self.channel.send(()).unwrap();
            match f(&mut *inner) {
                Some(r) => {
                    // We have done work, so again, notify the polling thread.
                    self.channel.send(()).unwrap();
                    return r;
                }
                None => {
                    tracing::trace!("blocking thread");
                    inner = self.waiter.wait(inner).unwrap();
                }
            }
        }
    }
}
