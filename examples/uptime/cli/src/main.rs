use cli::net::Net;
use std::sync::Arc;
use uptime_cli::{kit, wall, Clock};

fn main() {
    kit(Arc::new(Net), Arc::new(Clock(wall))).main()
}
