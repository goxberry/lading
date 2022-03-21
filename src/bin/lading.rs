use argh::{FromArgValue, FromArgs};
use lading::{
    blackhole,
    captures::CaptureManager,
    config::{Config, Telemetry},
    generator,
    signals::Shutdown,
    target,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::{collections::HashMap, io::Read};
use tokio::{
    runtime::Builder,
    signal,
    sync::broadcast,
    time::{sleep, Duration},
};
use tracing::{debug, info};

fn default_config_path() -> String {
    "/etc/lading/lading.yaml".to_string()
}

#[derive(Default)]
struct CliKeyValues {
    inner: HashMap<String, String>,
}

impl FromArgValue for CliKeyValues {
    fn from_arg_value(input: &str) -> Result<Self, String> {
        let pair_err = String::from("pairs must be separated by '='");
        let mut labels = HashMap::new();
        for kv in input.split(',') {
            let mut pair = kv.split('=');
            let key = pair.next().ok_or_else(|| pair_err.clone())?;
            let value = pair.next().ok_or_else(|| pair_err.clone())?;
            labels.insert(key.into(), value.into());
        }
        Ok(Self { inner: labels })
    }
}

#[derive(FromArgs)]
/// `lading` options
struct Opts {
    /// path on disk to the configuration file
    #[argh(option, default = "default_config_path()")]
    config_path: String,
    /// additional labels to apply to all captures, format KEY=VAL,KEY2=VAL
    #[argh(option, default = "CliKeyValues::default()")]
    global_labels: CliKeyValues,
    /// additional environment variables to apply to the target, format KEY=VAL,KEY2=VAL
    #[argh(option, default = "CliKeyValues::default()")]
    target_env_vars: CliKeyValues,
    /// path on disk to write captures, will override prometheus-addr if both
    /// are set
    #[argh(option)]
    capture_path: Option<String>,
    /// address to bind prometheus exporter to, will be overridden by
    /// capture-path if both are set
    #[argh(option)]
    prometheus_addr: Option<String>,
    /// the maximum time to wait, in seconds, for controlled shutdown
    #[argh(option, default = "10")]
    max_shutdown_delay: u16,
}

fn get_config() -> (Opts, Config) {
    let ops: Opts = argh::from_env();
    debug!(
        "Attempting to open configuration file at: {}",
        ops.config_path
    );
    let mut file: std::fs::File = std::fs::OpenOptions::new()
        .read(true)
        .open(&ops.config_path)
        .unwrap_or_else(|_| panic!("Could not open configuration file at: {}", &ops.config_path));
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let mut config: Config = serde_yaml::from_str(&contents).unwrap();
    for (k, v) in ops.target_env_vars.inner.clone() {
        config.target.environment_variables.insert(k, v);
    }
    if let Some(ref prom_addr) = ops.prometheus_addr {
        config.telemetry = Telemetry::Prometheus {
            prometheus_addr: prom_addr.parse().unwrap(),
            global_labels: ops.global_labels.inner.clone(),
        };
    } else if let Some(ref capture_path) = ops.capture_path {
        config.telemetry = Telemetry::Log {
            path: capture_path.parse().unwrap(),
            global_labels: ops.global_labels.inner.clone(),
        };
    } else {
        match config.telemetry {
            Telemetry::Prometheus {
                ref mut global_labels,
                ..
            } => {
                for (k, v) in ops.global_labels.inner.clone() {
                    global_labels.insert(k, v);
                }
            }
            Telemetry::Log {
                ref mut global_labels,
                ..
            } => {
                for (k, v) in ops.global_labels.inner.clone() {
                    global_labels.insert(k, v);
                }
            }
        }
    }
    (ops, config)
}

async fn inner_main(config: Config) {
    let (shutdown_snd, shutdown_rcv) = broadcast::channel(1);

    // Set up the telemetry sub-system.
    //
    // We support two methods to exflitrate telemetry about the target from rig:
    // a passive prometheus export and an active log file. Only one can be
    // active at a time.
    match config.telemetry {
        Telemetry::Prometheus {
            prometheus_addr,
            global_labels,
        } => {
            let mut builder = PrometheusBuilder::new().with_http_listener(prometheus_addr);
            for (k, v) in global_labels {
                builder = builder.add_global_label(k, v);
            }
            let _: () = builder.install().unwrap();
        }
        Telemetry::Log {
            path,
            global_labels,
        } => {
            let mut capture_manager =
                CaptureManager::new(path, Shutdown::new(shutdown_snd.subscribe())).await;
            capture_manager.install();
            for (k, v) in global_labels {
                capture_manager.add_global_label(k, v);
            }
            let _capmgr = tokio::spawn(capture_manager.run());
        }
    }

    // Set up the application servers. These are, depending on configuration:
    //
    // * the "generator" which pushes load into
    // * the "target" which is the measured system and might push load into
    // * the "blackhole" which may or may not exist.

    let generator_server =
        generator::Server::new(config.generator, Shutdown::new(shutdown_snd.subscribe())).unwrap();
    let _gsrv = tokio::spawn(generator_server.run());

    let target_server =
        target::Server::new(config.target, Shutdown::new(shutdown_snd.subscribe())).unwrap();
    let tsrv = tokio::spawn(target_server.run());

    if let Some(blackhole_conf) = config.blackhole {
        let blackhole_server =
            blackhole::Server::new(blackhole_conf, Shutdown::new(shutdown_snd.subscribe()));
        let _bsrv = tokio::spawn(blackhole_server.run());
    }

    // Tidy up our stray shutdown_rcv, avoiding a situation where we infinitely
    // wait to shut down.
    drop(shutdown_rcv);
    let experiment_duration = sleep(Duration::from_secs(config.experiment_duration.into()));

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("received ctrl-c");
            shutdown_snd.send(()).unwrap();
        },
        _ = experiment_duration => {
            info!("experiment duration exceeded");
            shutdown_snd.send(()).unwrap();
        }
        tgt = tsrv => {
            info!("{:?}", tgt);
            shutdown_snd.send(()).unwrap();
        }
    }

    loop {
        let remaining: usize = shutdown_snd.receiver_count();
        if remaining != 0 {
            info!("waiting for {} tasks to shutdown", remaining);
            // For reasons that are obscure to me if we sleep here it's
            // _possible_ for the runtime to fully lock up when the splunk_heck
            // -- at least -- generator is running. See note below. This only
            // seems to happen if we have a single-threaded runtime or a low
            // number of worker threads available. I've reproduced the issue
            // reliably with 2.
            sleep(Duration::from_secs(1)).await;
        } else {
            info!("all tasks shut down");
            return;
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    info!("Starting lading run.");
    let (opts, config): (Opts, Config) = get_config();
    let runtime = Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(inner_main(config));
    // The splunk_hec generator spawns long running tasks that are not plugged
    // into the shutdown mechanism we have here. This is a bug and needs to be
    // addressed. However as a workaround we explicitly shutdown the
    // runtime. Even when the splunk_hec issue is addressed we'll continue this
    // practice as it's a reasonable safeguard.
    info!(
        "Shutting down runtime with a {} second delay.",
        opts.max_shutdown_delay
    );
    runtime.shutdown_timeout(Duration::from_secs(opts.max_shutdown_delay.into()));
    info!("Bye. :)");
}
