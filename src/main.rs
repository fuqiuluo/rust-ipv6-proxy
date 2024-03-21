use std::future::Future;
use std::net::{Ipv6Addr, SocketAddr};
use std::str::{from_utf8, FromStr};
use std::string::String;
use bytes::BytesMut;
use clap::builder::styling::Reset;
use log::{info, warn, trace, error, set_logger, LevelFilter};
use clap::Parser;
use ipnetwork::Ipv6Network;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct ProxyConfig {
    /// Proxy Address
    #[arg(short, long, default_value = "127.0.0.1")]
    host: String,

    /// Proxy Port
    #[arg(short, long, default_value_t = 10880)]
    port: u16,

    /// Cidr
    #[arg(short, long)]
    cidr: String,
}

async fn random_ipv6(network: Ipv6Network) -> Ipv6Addr {
    let mut rng = rand::thread_rng();
    let network_start = u128::from_be_bytes(network.network().octets());
    let network_end = u128::from_be_bytes(network.broadcast().octets());
    let random_addr = rng.gen_range(network_start..=network_end);
    Ipv6Addr::from(random_addr)
}

fn handle_client(mut client: TcpStream, local_addr: Ipv6Addr) {
    tokio::spawn(async move {
        let (_ver, n_method) = (
            client.read_u8().await.map_err(|e| {
                error!("Unable to get proxy version: {}", e);
                return;
            }).unwrap(),
            client.read_u8().await.map_err(|e| {
                error!("Unable to get proxy n_method: {}", e);
                return;
            }).unwrap()
        );

        let mut methods = vec![0u8; n_method as usize];
        if let Err(e) = client.read_exact(&mut methods).await {
            error!("Failed to fetch methods: {}", e);
            return;
        }


        if let Err(e) = client.write_u16(((5u16) << 8) | (0u16)).await {
            error!("Failed to response proxy info: {}", e);
            return;
        }
        //sock.write_u8(5).await.unwrap(); // proxy version?
        //sock.write_u8(0).await.unwrap(); // proxy auth method?

        let mut buffer = [0u8; 4];
        if let Err(e) = client.read_exact(&mut buffer).await {
            error!("Failed to fetch proxy info: {}", e);
            return;
        }
        let (_ver, cmd, _, address_type) = (buffer[0], buffer[1], buffer[2], buffer[3]);

        if cmd != 1 {
            // Only support the CONNECT command
            return;
        }

        if address_type == 1 {  // ipv4
            // Only support ipv6
            return;
        }

        let address;
        let port;

        if address_type == 3 { // ipv6
            let domain_length = client.read_u8().await.unwrap() as usize;
            let mut domain_buf = vec![0u8; domain_length];
            client.read_exact(&mut domain_buf).await.unwrap();
            address = String::from_utf8(domain_buf)
                .expect("The domain is UTF-8 string?");
        } else if address_type == 4 {
            let mut ipv6_buffer = vec![0u8; 16];
            client.read_exact(&mut ipv6_buffer).await.unwrap();
            address = String::from_utf8(ipv6_buffer)
                .expect("The ipv6 is UTF-8 string?");
        } else {
            error!("Unable to get remote address!!!");
            return;
        }

        {
            let mut port_buf = [0u8; 2];
            if let Err(e) = client.read_exact(&mut port_buf).await {
                error!("Failed to fetch proxy info (port): {}", e);
                return;
            }
            port = u16::from_le_bytes(port_buf.try_into()
                .expect("At least 2 bytes are required"))
        }

        info!("Generated ipv6: {}", local_addr);

        let local_socket_addr = SocketAddr::new(local_addr.into(), 0);
        let socket = match TcpSocket::new_v6() {
            Ok(result) => result,
            Err(e) => {
                warn!("Failed to create connection({}:{}), err: {}", address, port, e);
                return;
            }
        };
        if let Err(e) = socket.bind(local_socket_addr) {
            warn!("Failed to bind {}:{}, err: {}", address, port, e);
            return;
        }

        info!("Connecting to {}:{} via {}", address, port, local_addr);
        let mut target = match socket.connect(
            format!("{}:{}", address, port).parse().unwrap()
        ).await {
            Ok(result) => result,
            Err(e) => {
                warn!("Failed to connect to {}:{}, err: {}", address, port, e);
                return;
            }
        };

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[5u8, 0, 0, 1]); // 5 0 0 1 1
        buf.extend_from_slice(&1u32.to_be_bytes()); // 1
        buf.extend_from_slice(&1u16.to_be_bytes()); // 1
        if let Err(e) = client.write_all(&buf).await {
            error!("Failed to send success msg to the client! err: {}", e);
            return;
        }
        if let Err(e) = client.flush().await {
            error!("Failed to send success msg to the client! err: {}", e);
            return;
        }

        //let (mut client_r, mut client_w) = client.split();
        //let (mut target_r, mut target_w) = target.split();

        //let copy1 = tokio::io::copy(&mut client_r, &mut target_w);
        //let copy2 = tokio::io::copy(&mut target_r, &mut client_w);

        match tokio::try_join!(tokio::io::copy_bidirectional(&mut target, &mut client)) {
            Ok((_)) => {}
            Err(e) => {
                error!("Error during the copy operation: {}", e);
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder()
        .filter_level(LevelFilter::Info)
        .is_test(true)
        .try_init().unwrap();

    let config = ProxyConfig::parse();

    info!("Proxy are running on {}:{}", config.host, config.port);

    let listener = match TcpListener::bind(format!("{}:{}", config.host, config.port)).await {
        Ok(result) => result,
        Err(e) =>  {
            error!("Failed to start server on {}:{}", config.host, config.port);
            return Err(Box::try_from(e).unwrap());
        }
    };

    loop {
        match listener.accept().await {
            Ok((sock, remote_addr)) => {
                info!("Connect to {}", remote_addr);

                let local_addr = random_ipv6(config.cidr.parse().unwrap()).await;
                handle_client(sock, local_addr);
            }
            Err(e) => { error!("Failed to accept connection: {}", e); }
        };
    }
}
