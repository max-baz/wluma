use futures_util::stream::{self, StreamExt};
use macro_rules_attribute::apply;
use smol::channel;

mod als;
mod brightness;
mod channel_ext;
mod config;
mod device_file;
mod frame;
mod predictor;

/// Current app version (determined at compile-time).
pub const VERSION: &str = env!("WLUMA_VERSION");

#[apply(smol_macros::main!)]
async fn main() {
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        panic_hook(panic_info);
        std::process::exit(1);
    }));

    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    log::debug!("== wluma v{} ==", VERSION);

    let config = match config::load() {
        Ok(config) => config,
        Err(err) => panic!("Unable to load config: {}", err),
    };

    log::debug!("Using {:#?}", config);

    let (mut tasks, als_txs) = stream::iter(config.output.clone())
        .fold(
            (Vec::new(), Vec::new()),
            |(mut tasks, mut als_txs), output| async move {
                let output_clone = output.clone();

                let (als_tx, als_rx) = channel::bounded(128);
                let (user_tx, user_rx) = channel::bounded(128);
                let (prediction_tx, prediction_rx) = channel::bounded(128);

                let (output_name, output_capturer) = match output_clone.clone() {
                    config::Output::Backlight(cfg) => (cfg.name, cfg.capturer),
                    config::Output::DdcUtil(cfg) => (cfg.name, cfg.capturer),
                };

                let brightness = match output {
                    config::Output::Backlight(cfg) => {
                        brightness::Backlight::new(&cfg.path, cfg.min_brightness)
                            .await
                            .map(brightness::Brightness::Backlight)
                    }
                    config::Output::DdcUtil(cfg) => {
                        brightness::DdcUtil::new(&cfg.identifier, cfg.min_brightness)
                            .map(brightness::Brightness::DdcUtil)
                    }
                };

                match brightness {
                    Ok(b) => {
                        tasks.push(smol::spawn(async {
                            brightness::Controller::new(b, user_tx, prediction_rx)
                                .run()
                                .await;
                        }));

                        let predictor = match output_clone.clone() {
                            config::Output::Backlight(backlight_output) => {
                                backlight_output.predictor
                            }
                            config::Output::DdcUtil(ddcutil_output) => ddcutil_output.predictor,
                        };

                        tasks.push(smol::spawn(async move {
                            let frame_capturer: frame::capturer::Capturer = match output_capturer {
                                config::Capturer::Wayland(protocol) => {
                                    frame::capturer::Capturer::Wayland(
                                        frame::capturer::wayland::Capturer::new(protocol),
                                    )
                                }
                                config::Capturer::None => {
                                    frame::capturer::Capturer::None(Default::default())
                                }
                            };

                            let controller = match predictor {
                                config::Predictor::Manual { thresholds } => {
                                    predictor::Controller::Manual(
                                        predictor::controller::manual::Controller::new(
                                            prediction_tx,
                                            user_rx,
                                            als_rx,
                                            thresholds,
                                            &output_name,
                                        ),
                                    )
                                }
                                config::Predictor::Adaptive => predictor::Controller::Adaptive(
                                    predictor::controller::adaptive::Controller::new(
                                        prediction_tx,
                                        user_rx,
                                        als_rx,
                                        true,
                                        &output_name,
                                    ),
                                ),
                            };

                            frame_capturer.run(&output_name, controller).await;
                        }));

                        als_txs.push(als_tx);
                    }
                    Err(err) => {
                        log::warn!(
                            "Skipping '{}' as it might be disconnected: {}",
                            output_name,
                            err
                        );
                    }
                }

                (tasks, als_txs)
            },
        )
        .await;

    let als: als::Als = match config.als {
        config::Als::Iio { path, thresholds } => als::Als::Iio(
            als::iio::Als::new(&path, thresholds)
                .await
                .expect("Unable to initialize ALS IIO sensor"),
        ),
        config::Als::Time { thresholds } => als::Als::Time(als::time::Als::new(thresholds)),
        config::Als::Webcam { video, thresholds } => als::Als::Webcam({
            let (webcam_tx, webcam_rx) = channel::bounded(128);
            // TODO: make async
            tasks.push(smol::unblock(move || {
                als::webcam::Webcam::new(webcam_tx, video).run();
            }));
            als::webcam::Als::new(webcam_rx, thresholds)
        }),
        config::Als::None => als::Als::None(Default::default()),
    };

    tasks.push(smol::spawn(async {
        als::controller::Controller::new(als, als_txs).run().await;
    }));

    log::info!("Continue adjusting brightness and wluma will learn your preference over time.");

    futures_util::future::join_all(tasks).await;
}
