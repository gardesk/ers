//! CLI configuration for ers.

#[derive(Debug, Clone)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0) as f64 / 255.0;
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0) as f64 / 255.0;
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0) as f64 / 255.0;
        let a = if hex.len() >= 8 {
            u8::from_str_radix(&hex[6..8], 16).unwrap_or(255) as f64 / 255.0
        } else {
            1.0
        };
        Self { r, g, b, a }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub border_width: f64,
    pub active_color: Color,
    pub inactive_color: Color,
    pub radius: f64,
    pub hidpi: bool,
    pub border_order: i32,
    pub active_only: bool,
    pub standalone: bool,
    pub tarmac_socket: Option<String>,
    pub test_wid: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            border_width: 4.0,
            active_color: Color::from_hex("#5294e2"),
            inactive_color: Color::from_hex("#2d2d2d80"),
            radius: 10.0,
            hidpi: true,
            border_order: -1, // below target by default (like JankyBorders)
            active_only: false,
            standalone: false,
            tarmac_socket: None,
            test_wid: None,
        }
    }
}

impl Config {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config = Self::default();
        let mut i = 1;

        while i < args.len() {
            match args[i].as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--version" | "-v" => {
                    println!("ers {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--width" | "-w" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.border_width = v.parse().unwrap_or(config.border_width);
                    }
                }
                "--color" | "-c" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.active_color = Color::from_hex(v);
                    }
                }
                "--inactive" | "-i" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.inactive_color = Color::from_hex(v);
                    }
                }
                "--radius" | "-r" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.radius = v.parse().unwrap_or(config.radius);
                    }
                }
                "--active-only" => {
                    config.active_only = true;
                }
                "--standalone" => {
                    config.standalone = true;
                }
                "--tarmac" => {
                    i += 1;
                    config.tarmac_socket = args.get(i).cloned();
                }
                "--test-wid" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.test_wid = v.parse().ok();
                    }
                }
                "--order" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        config.border_order = match v.as_str() {
                            "above" => 1,
                            "below" => -1,
                            _ => v.parse().unwrap_or(-1),
                        };
                    }
                }
                "--no-hidpi" => {
                    config.hidpi = false;
                }
                other => {
                    eprintln!("unknown option: {other}");
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        config
    }
}

fn print_help() {
    println!(
        "\
ers — window border renderer

USAGE:
    ers [OPTIONS]

OPTIONS:
    -w, --width <px>        Border width in pixels (default: 4)
    -c, --color <hex>       Active border color (default: #5294e2)
    -i, --inactive <hex>    Inactive border color (default: #2d2d2d80)
    -r, --radius <px>       Corner radius (default: 10)
        --order <mode>      Border order: above or below (default: below)
        --active-only       Only draw border on focused window
        --standalone        Use SLS focus detection (no tarmac IPC)
        --tarmac <path>     Path to tarmac Unix socket
        --test-wid <id>     Draw border on a specific window ID and exit
        --no-hidpi          Disable HiDPI (2x) rendering
    -h, --help              Show this help
    -v, --version           Show version"
    );
}
