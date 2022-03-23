//! Load tests connection attempts to FreeSWITCH.

use anyhow::Result;
use clap::Parser;
use free_socks::EventSocket;
use std::time::{Duration, Instant};
use tokio::time;

#[derive(Parser, Debug)]
struct Args {
    /// The endpoint to connect to.
    endpoint: String,

    /// The port to connect to.
    #[clap(long, default_value_t = 8021)]
    port: usize,

    /// The password to use.
    #[clap(short, long, default_value_t = String::from("ClueCon"))]
    password: String,

    /// Max connections per second.
    #[clap(long, default_value_t = 1)]
    cps: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut attempts_made = 0;

    let start_time = Instant::now();

    loop {
        let endpoint = format!("{}:{}", args.endpoint, args.port);
        let begin_connecting = Instant::now();
        let event_socket = EventSocket::connect(&endpoint).await?;
        event_socket.auth(&args.password).await?;
        println!("Time to connect: {:?}", Instant::now() - begin_connecting);
        attempts_made += 1;
        let time_of_next_attempt = start_time + (Duration::from_secs(1) * attempts_made / args.cps);
        let now = Instant::now();
        if now > time_of_next_attempt {
            println!(
                "Not catching up, delayed by: {:?}",
                now - time_of_next_attempt
            );
        } else {
            time::sleep_until(time_of_next_attempt.into()).await
        }
    }
}
