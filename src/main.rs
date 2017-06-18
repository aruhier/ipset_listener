#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
extern crate config;
extern crate regex;
extern crate rustc_serialize;

mod conf;
mod multisocketaddr;

use regex::Regex;
use std::error::Error;
use std::io::prelude::{Read, Write};
use std::net::{self, IpAddr, TcpStream, TcpListener, ToSocketAddrs};
use std::process::Command;
use std::sync::{Arc, Mutex, Condvar};
use std::thread;
use std::time::Duration;

use conf::{GLOBAL_CONFIG, SetIpset};
use multisocketaddr::MultiSocketAddr;

lazy_static! {
    static ref REGISTERED_USERS_SET: &'static SetIpset = &(
        GLOBAL_CONFIG.registered_users_set
    );
}
static RE_MAC_PATTERN: &'static str = (
    r"(?P<mac>([a-f\d]{1,2}:){5}[a-f\d]{1,2})"
);

/// Is an IP address ?
///
/// returns bool
fn is_ip_addr(s: &str) -> bool {
    match s.parse::<IpAddr>() {
        Ok(_) => return true,
        Err(_) => return false
    }
}


/// Create our set in ipset
fn create_ipset_set() -> Result<(), String> {
    debug!("Creates set {} in ipset.", REGISTERED_USERS_SET.name);
    let panic_err = |e: &str| -> String {
        let msg: String = format!(
            "Failed to create {} in ipset", REGISTERED_USERS_SET.name
        );
        error!("{}: {}", msg, e);
        msg
    };
    let creation = match Command::new(&GLOBAL_CONFIG.ipset_bin)
        .arg("create").arg("-exist")
        .arg(&REGISTERED_USERS_SET.name)
        .arg(&REGISTERED_USERS_SET.type_name)
        .arg("maxelem").arg(REGISTERED_USERS_SET.maxelem.to_string())
        .output() {
            Ok(p) => p,
            Err(err) => return Err(panic_err(err.description().trim_right())),
        };
    if ! creation.status.success() {
        return Err(panic_err(
            &String::from_utf8(creation.stderr).unwrap().trim_right()
        ));
    }

    Ok(())
}


/// Interacts with ipset
///
/// First creates the set, with -exist to avoid any error if the wanted set
/// already exists, then executes ipset with arguments received in parameter
///
/// ipset_args <&[&str]>: arguments for ipset
fn spawn_ipset(ipset_args: &[&str]) -> Result<(), String> {
    // Ensure that our set exists in ipset
    match create_ipset_set() {
        Ok(()) => {},
        Err(err) => return Err(err),
    }

    debug!("Launch \"{} {}\"", GLOBAL_CONFIG.ipset_bin, ipset_args.join(" "));
    let panic_err = |e: &str| {
        let msg: String = format!(
            "Failed to launch \"{} {}\"",
            GLOBAL_CONFIG.ipset_bin, ipset_args.join(" ")
        );
        error!("{}: {}", msg, e);
        msg
    };
    let launch_cmd = match Command::new(&GLOBAL_CONFIG.ipset_bin)
        .args(ipset_args)
        .output() {
            Ok(p) => p,
            Err(err) => return Err(panic_err(err.description().trim_right())),
        };
    if ! launch_cmd.status.success() {
        return Err(panic_err(
            &String::from_utf8(launch_cmd.stderr).unwrap().trim_right()
        ));
    }

    Ok(())
}


/// Apply a regex on the "ip neigh" output to get the mac_address
///
/// output <&str>: "ip neigh" output
fn filter_mac(output: &str) -> Result<String, String> {
    let re_mac: Regex = Regex::new(RE_MAC_PATTERN).unwrap();
    let mac_addr = match re_mac.captures(output) {
        Some(capt) => capt.name("mac").unwrap_or(""),
        None => "",
    };
    match mac_addr {
        "" => Err(String::from("MAC cannot be found")),
        m => Ok(String::from(m)),
    }
}


/// Look for all mac addresses linked to the sent IP
///
/// ip <&str>: arguments for ipset
fn get_mac<'a>(ip: &'a str) -> Result<String, String> {
    let ip_bin = "ip";
    let ip_args = ["n", "show", "to", ip];

    debug!("Launch \"{} {}\"", ip_bin, ip_args.join(" "));
    let panic_err = |e: &str| {
        let msg: String = format!(
            "Failed to launch \"{} {}\"", ip_bin, ip_args.join(" ")
        );
        error!("{}: {}", msg, e);
        msg
    };

    let launch_cmd = match Command::new(ip_bin).args(&ip_args)
        .output() {
            Ok(p) => p,
            Err(err) => return Err(panic_err(err.description().trim_right())),
        };
    if launch_cmd.status.success() {
        let mac_addr_result = filter_mac(
            String::from_utf8(launch_cmd.stdout).unwrap().trim_right()
        );
        return match mac_addr_result {
            Ok(m) => Ok(m),
            Err(e) => Err(panic_err(e.as_str())),
        }
    }
    else {
        return Err(panic_err(
            String::from_utf8(launch_cmd.stderr).unwrap().trim_right()
        ))
    }
}


/// Checks if the response is correct and parse it
fn compute_response(response: &String, mut s: &TcpStream) {
    let re_action: Regex = Regex::new(
        r"^(?P<action>[:alpha:]) *(?P<arg>.*)$"
    ).unwrap();
    let re_mac: Regex = Regex::new(RE_MAC_PATTERN).unwrap();

    let send_error = |mut s: &TcpStream, err: &str| {
        s.write(&(format!("1 {}\r\n", err.trim_right())).into_bytes()).unwrap()
    };

    let mut bad_request: bool = false;
    match re_action.captures(response.as_str()) {
        Some(capt) => {
            let action = capt.name("action").unwrap_or("");
            let arg = capt.name("arg").unwrap_or("");
            info!("{:?}", (action, arg));

            match action {
                act_ipset @ "a" | act_ipset@ "d" => {
                    let mac_addr = match re_mac.captures(arg) {
                        Some(mac_capt) => mac_capt.name("mac").unwrap_or(""),
                        None => { bad_request = true; "" },
                    };
                    let cmd = match act_ipset {
                        "a" => "add",
                        "d" => "del",
                        _ => panic!("Action doesn't match"),
                    };
                    if mac_addr != "" {
                        match spawn_ipset(
                            &[
                                cmd, "-exist",
                                &REGISTERED_USERS_SET.name, mac_addr
                            ]
                        ) {
                            Ok(()) => { s.write(b"0\r\n").unwrap(); },
                            Err(err) => { send_error(&s, err.as_str()); },
                        };
                    }
                }, "m" => {
                    if is_ip_addr(arg) {
                        let ipaddr = arg;
                        match get_mac(ipaddr) {
                            Ok(mac) => {
                                let response = format!(
                                    "0 {}\r\n", mac
                                ).into_bytes();
                                s.write(&response).unwrap();
                            }
                            Err(err) => { send_error(&s, err.as_str()); },
                        };
                    }
                    else {
                        send_error(&s, "Not an IP address");
                    }
                }, _ => bad_request = true,
            }
        }, None => bad_request = true,
    }

    if bad_request {
        let msg: String = format!(
            "\"{}\": Request doesn't respect the protocol", response
        );
        error!("{}", msg.as_str());
        send_error(&s, msg.as_str());
    }
}


/// Handle a new client and call to compute the response
///
/// s <TcpStream>: client's stream
fn handle_client(s: &TcpStream) {
    let mut response: String = String::new();
    for b_result in s.bytes() {
        let b: u8 = b_result.unwrap();
        response.push(b as char);
        // End of line. Parse the received request.
        if b == 10 {
            response = String::from(response.trim());
            compute_response(&response, s);
            response.clear();
        }
    }

    if response.len() > 0 {
        response = String::from(response.trim());
        compute_response(&response, s);
    }
}


/// Create a TcpListener for the sent addr
///
/// addr <SocketAddr>: Address to bind on
/// nb_threads_arc <Arc<(Mutex<u32>, Condvar)>>:
///     used to limit the number of threads spawned
fn listen_on_addr(addr: net::SocketAddr,
                  nb_threads_arc: Arc<(Mutex<u32>, Condvar)>) {
    let listener = TcpListener::bind(addr).unwrap();
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Requests should be snappy enough to never reach the 60
                // seconds of timeout. If they reach it, we have another
                // problem somewhere else…
                {
                    let timeout = Some(Duration::new(60, 0));
                    let _ = stream.set_read_timeout(timeout);
                    let _ = stream.set_write_timeout(timeout);
                }
                // Checks if we have not already spawned the maximum threads
                // allowed
                let nb_threads_arc = nb_threads_arc.clone();
                {
                    let &(ref lock, ref cvar) = &*nb_threads_arc;
                    let mut nb_threads = lock.lock().unwrap();
                    // If we reached the limit, wait until any thread exits
                    while *nb_threads >= GLOBAL_CONFIG.limit_threads {
                        nb_threads = cvar.wait(nb_threads).unwrap();
                    }
                    debug!("{}", *nb_threads);
                    *nb_threads += 1;
                }
                thread::spawn(move || {
                    let &(ref lock, ref cvar) = &*nb_threads_arc;
                    println!("New client…");
                    handle_client(&stream);
                    {
                        let mut nb_threads = lock.lock().unwrap();
                        *nb_threads -= 1;
                        debug!("{}", *nb_threads);
                    }
                    // Notifies one waiting thread that the current one is
                    // exiting
                    cvar.notify_one();
                });
            },
            Err(_) => {
                break
            }
        }
    }
    drop(listener);
}


fn main() {
    extern crate env_logger;
    let _ = env_logger::init();

    let mut multi = MultiSocketAddr::new();
    for addr in GLOBAL_CONFIG.listen_addr.iter() {
        multi.add(addr).unwrap();
    }

    // As we want to bind on several SocketAddr, spawns one listener by
    // SocketAddr in its own thread
    let nb_threads_arc = Arc::new((Mutex::new(0u32), Condvar::new()));
    let mut listeners = Vec::new();
    for addr in multi.to_socket_addrs().unwrap() {
        let nb_threads_arc = nb_threads_arc.clone();
        listeners.push(thread::spawn(move || {
            listen_on_addr(addr, nb_threads_arc);
        }));
    }

    // Wait for threads to finish
    for l in listeners {
        let _ = l.join();
    }
}
