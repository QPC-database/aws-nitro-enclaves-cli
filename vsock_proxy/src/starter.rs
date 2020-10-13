// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
#![deny(warnings)]

/// Contains code for Proxy, a library used for translating vsock traffic to
/// TCP traffic
///
use dns_lookup::lookup_host;
use idna;
use log::info;
use nix::sys::select::{select, FdSet};
use nix::sys::socket::{SockAddr, SockType};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use threadpool::ThreadPool;
use vsock::VsockListener;
use yaml_rust::YamlLoader;

const BUFF_SIZE: usize = 8192;
pub const VSOCK_PROXY_CID: u32 = 3;
pub const VSOCK_PROXY_PORT: u32 = 8000;

/// The most common result type provided by VsockProxy operations.
pub type VsockProxyResult<T> = Result<T, String>;

/// Checks if the forwarded server is allowed
pub fn check_allowlist(
    remote_addr: IpAddr,
    remote_port: u16,
    config_file: Option<&str>,
    only_4: bool,
    only_6: bool,
) -> VsockProxyResult<()> {
    if let Some(config_file) = config_file {
        let mut f = File::open(config_file).map_err(|_| "Could not open the file")?;

        let mut content = String::new();
        f.read_to_string(&mut content)
            .map_err(|_| "Could not read the file")?;

        let docs = YamlLoader::load_from_str(&content).map_err(|_| "Bad yaml format")?;
        let services = (&docs[0])["allowlist"]
            .as_vec()
            .ok_or_else(|| "No allowlist field")?;

        for raw_service in services {
            let port = raw_service["port"]
                .as_i64()
                .ok_or_else(|| "No port field or invalid type")?;
            let port = port as u16;

            let addr = raw_service["address"]
                .as_str()
                .ok_or_else(|| "No address field")?;
            let addrs = Proxy::parse_addr(addr, only_4, only_6)?;
            for addr in addrs.into_iter() {
                if addr == remote_addr && port == remote_port {
                    return Ok(());
                }
            }
        }
    }
    Err("The given address and port are not allowed".to_string())
}

/// Configuration parameters for port listening and remote destination
pub struct Proxy {
    local_port: u32,
    remote_addr: IpAddr,
    remote_port: u16,
    pool: ThreadPool,
    sock_type: SockType,
}

impl Proxy {
    pub fn new(
        local_port: u32,
        remote_addr: IpAddr,
        remote_port: u16,
        num_workers: usize,
        config_file: Option<&str>,
        only_4: bool,
        only_6: bool,
    ) -> VsockProxyResult<Self> {
        if num_workers == 0 {
            return Err("Number of workers must not be 0".to_string());
        }
        info!("Checking allowlist configuration");
        check_allowlist(remote_addr, remote_port, config_file, only_4, only_6)
            .map_err(|err| format!("Error at checking the allowlist: {}", err))?;

        let pool = ThreadPool::new(num_workers);
        let sock_type = SockType::Stream;
        Ok(Proxy {
            local_port,
            remote_addr,
            remote_port,
            pool,
            sock_type,
        })
    }

    /// Resolve a DNS name (IDNA format) into an IP address (v4 or v6)
    pub fn parse_addr(addr: &str, only_4: bool, only_6: bool) -> VsockProxyResult<Vec<IpAddr>> {
        // IDNA parsing
        let addr = idna::domain_to_ascii(&addr).map_err(|_| "Could not parse domain name")?;

        // DNS lookup
        // It results in a vector of IPs (V4 and V6)
        let ips = match lookup_host(&addr) {
            Err(_) => {
                return Ok(vec![]);
            }
            Ok(v) => {
                if v.is_empty() {
                    return Ok(v);
                }
                v
            }
        };

        // If there is no restriction, choose randomly
        if !only_4 && !only_6 {
            return Ok(ips.into_iter().collect());
        }

        // Split the IPs in v4 and v6
        let (ips_v4, ips_v6): (Vec<_>, Vec<_>) = ips.into_iter().partition(IpAddr::is_ipv4);

        if only_4 && !ips_v4.is_empty() {
            Ok(ips_v4.into_iter().collect())
        } else if only_6 && !ips_v6.is_empty() {
            Ok(ips_v6.into_iter().collect())
        } else {
            Err("No accepted IP was found".to_string())
        }
    }

    /// Creates a listening socket
    /// Returns the file descriptor for it or the appropriate error
    pub fn sock_listen(&self) -> VsockProxyResult<VsockListener> {
        let sockaddr = SockAddr::new_vsock(VSOCK_PROXY_CID, self.local_port);
        let listener = VsockListener::bind(&sockaddr)
            .map_err(|_| format!("Could not bind to {:?}", sockaddr))?;
        info!("Bound to {:?}", sockaddr);

        Ok(listener)
    }

    /// Accepts an incoming connection coming on listener and handles it on a
    /// different thread
    /// Returns the handle for the new thread or the appropriate error
    pub fn sock_accept(&self, listener: &VsockListener) -> VsockProxyResult<()> {
        let (mut client, client_addr) = listener
            .accept()
            .map_err(|_| "Could not accept connection")?;
        info!("Accepted connection on {:?}", client_addr);

        let sockaddr = SocketAddr::new(self.remote_addr, self.remote_port);
        let sock_type = self.sock_type;
        self.pool.execute(move || {
            let mut server = match sock_type {
                SockType::Stream => TcpStream::connect(sockaddr)
                    .map_err(|_| format!("Could not connect to {:?}", sockaddr)),
                _ => Err("Socket type not implemented".to_string()),
            }
            .expect("Could not create connection");
            info!("Connected client from {:?} to {:?}", client_addr, sockaddr);

            let client_socket = client.as_raw_fd();
            let server_socket = server.as_raw_fd();

            let mut disconnected = false;
            while !disconnected {
                let mut set = FdSet::new();
                set.insert(client_socket);
                set.insert(server_socket);

                select(None, Some(&mut set), None, None, None).expect("select");

                if set.contains(client_socket) {
                    disconnected = transfer(&mut client, &mut server);
                }
                if set.contains(server_socket) {
                    disconnected = transfer(&mut server, &mut client);
                }
            }
            info!("Client on {:?} disconnected", client_addr);
        });

        Ok(())
    }
}

/// Transfers a chunck of maximum 4KB from src to dst
/// If no error occurs, returns true if the source disconnects and false otherwise
fn transfer(src: &mut dyn Read, dst: &mut dyn Write) -> bool {
    let mut buffer = [0u8; BUFF_SIZE];

    let nbytes = src.read(&mut buffer);
    let nbytes = match nbytes {
        Err(_) => 0,
        Ok(n) => n,
    };

    if nbytes == 0 {
        return true;
    }

    dst.write_all(&buffer[..nbytes]).is_err()
}

#[cfg(test)]
mod tests {
    use rand;
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;

    use super::*;

    /// Test transfer function with more data than buffer
    #[test]
    fn test_transfer() {
        let data: Vec<u8> = (0..2 * BUFF_SIZE).map(|_| rand::random::<u8>()).collect();

        let _ret = fs::create_dir("tmp");
        let mut src = File::create("tmp/src").unwrap();
        let mut dst = File::create("tmp/dst").unwrap();

        let _ret = src.write_all(&data);

        let mut src = File::open("tmp/src").unwrap();
        while !transfer(&mut src, &mut dst) {}

        let status = Command::new("cmp")
            .arg("tmp/src")
            .arg("tmp/dst")
            .status()
            .expect("command");

        let _ret = fs::remove_dir_all("tmp");

        assert!(status.success());
    }
}
