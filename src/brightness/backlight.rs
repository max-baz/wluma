use super::{validate_limits, BackendChanged};
use crate::device_file::{read, write};
use anyhow::{anyhow, Error, Result};
use dbus::channel::Sender;
use dbus::{self, blocking::Connection, Message};
use inotify::{Inotify, WatchMask};
use smol::fs::{File, OpenOptions};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BRIGHTNESS_STEPS: u64 = 1000;
const POLLED_WRITE_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);

fn requires_polling(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|subsystem| subsystem == "leds")
}

fn settle_polled_write(
    pending: &mut Option<(u64, Instant)>,
    observed: u64,
    cached: Option<u64>,
    now: Instant,
) -> u64 {
    if let Some((expected, deadline)) = *pending {
        if observed == expected || now >= deadline {
            *pending = None;
        } else {
            return cached.unwrap_or(observed);
        }
    }
    observed
}

struct Dbus {
    connection: Connection,
    message: Message,
}

pub struct Backlight {
    file: File,
    path: PathBuf,
    identity: (u64, u64),
    min_brightness: u64,
    max_brightness: u64,
    inotify: Inotify,
    current: Option<u64>,
    dbus: Option<Dbus>,
    has_write_permission: bool,
    pending_polled_write: Option<(u64, Instant)>,
    poll_brightness: bool,
}

impl Backlight {
    pub async fn new(path: &str, min_brightness: u64) -> Result<Self> {
        let brightness_path = Path::new(path).join("brightness");
        let max_brightness = fs::read_to_string(Path::new(path).join("max_brightness"))?
            .trim()
            .parse()?;
        validate_limits(min_brightness, max_brightness)?;
        let initial_metadata = fs::metadata(&brightness_path)?;
        let identity = (initial_metadata.dev(), initial_metadata.ino());

        let has_write_permission = fs::OpenOptions::new()
            .write(true)
            .open(&brightness_path)
            .is_ok();

        let (file, dbus) = if has_write_permission {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&brightness_path)
                .await?;

            log::debug!("Using direct write on {} to change brightness value", path);
            (file, None)
        } else {
            let file = File::open(&brightness_path).await?;

            let id = Path::new(path)
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or(anyhow!("Unable to identify backlight ID"))?;

            let subsystem = Path::new(path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|x| x.to_str())
                .and_then(|x| match x {
                    "backlight" | "leds" => Some(x),
                    _ => None,
                })
                .ok_or(anyhow!(
                    "Unable to identify backlight subsystem out of {path}, please open an issue on GitHub"
                ))?;

            let message = Message::new_method_call(
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
                "SetBrightness",
            )
            .ok()
            .map(|m| m.append2(subsystem, id));

            let connection = Connection::new_system().ok().and_then(|connection| {
                message.map(|message| Dbus {
                    connection,
                    message,
                })
            });

            log::debug!("Using DBUS for {} to change brightness value", path);
            (file, connection)
        };

        let opened_metadata = file.metadata().await?;
        anyhow::ensure!(
            identity == (opened_metadata.dev(), opened_metadata.ino()),
            "Backlight changed during initialization"
        );

        let inotify = Inotify::init()?;
        inotify.watches().add(&brightness_path, WatchMask::MODIFY)?;

        let brightness_hw_changed_path = Path::new(path).join("brightness_hw_changed");
        if Path::new(&brightness_hw_changed_path).exists() {
            inotify
                .watches()
                .add(&brightness_hw_changed_path, WatchMask::MODIFY)?;
        }

        let poll_brightness = requires_polling(Path::new(path));

        let backend = Self {
            file,
            path: PathBuf::from(path),
            identity,
            min_brightness,
            max_brightness,
            inotify,
            current: None,
            dbus,
            has_write_permission,
            pending_polled_write: None,
            poll_brightness,
        };
        backend.validate()?;
        Ok(backend)
    }

    pub fn validate(&self) -> Result<()> {
        let check = || -> Result<()> {
            let metadata = fs::metadata(self.path.join("brightness"))?;
            anyhow::ensure!(
                self.identity == (metadata.dev(), metadata.ino()),
                "Backlight device was replaced"
            );
            let maximum: u64 = fs::read_to_string(self.path.join("max_brightness"))?
                .trim()
                .parse()?;
            validate_limits(self.min_brightness, maximum)?;
            anyhow::ensure!(
                maximum == self.max_brightness,
                "Backlight maximum changed from {} to {maximum}",
                self.max_brightness
            );
            Ok(())
        };
        check().map_err(|error| {
            BackendChanged(format!(
                "Backlight '{}' is no longer valid: {error:#}",
                self.path.display()
            ))
            .into()
        })
    }

    pub async fn get(&mut self) -> Result<u64> {
        self.validate()?;
        async fn update(this: &mut Backlight) -> Result<u64> {
            let value = read(&mut this.file).await? as u64;
            this.current = Some(value.clamp(this.min_brightness, this.max_brightness));
            Ok(value)
        }

        if self.poll_brightness {
            let observed = read(&mut self.file).await? as u64;
            let value = settle_polled_write(
                &mut self.pending_polled_write,
                observed,
                self.current,
                Instant::now(),
            );
            self.current = Some(value.clamp(self.min_brightness, self.max_brightness));
            return Ok(value);
        }

        let mut buffer = [0u8; 1024];
        match (self.inotify.read_events(&mut buffer), self.current) {
            (_, None) => update(self).await,
            (Ok(mut events), Some(cached)) => {
                if events.next().is_none() {
                    Ok(cached)
                } else {
                    update(self).await
                }
            }
            (Err(err), Some(cached)) if err.kind() == ErrorKind::WouldBlock => Ok(cached),
            (Err(err), _) => Err(err.into()),
        }
    }

    pub fn min(&self) -> u64 {
        self.min_brightness
    }

    pub fn max(&self) -> u64 {
        self.max_brightness
    }

    pub fn change_threshold(&self) -> u64 {
        self.max_brightness
            .saturating_sub(self.min_brightness)
            .div_ceil(BRIGHTNESS_STEPS)
            .max(1)
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        self.validate()?;
        let value = value.clamp(self.min_brightness, self.max_brightness);

        if self.has_write_permission {
            write(&mut self.file, value as f64).await?;
        } else if let Some(dbus) = &self.dbus {
            let mut message = dbus
                .message
                .duplicate()
                .map_err(Error::msg)?
                .append1(value as u32);
            message.set_no_reply(true);
            dbus.connection
                .send(message)
                .map_err(|_| anyhow!("Unable to send brightness change message via dbus"))?;
        } else {
            Err(std::io::Error::from(ErrorKind::PermissionDenied))?
        }

        if self.poll_brightness {
            self.pending_polled_write = Some((value, Instant::now() + POLLED_WRITE_SETTLE_TIMEOUT));
        }
        self.current = Some(value);

        // Consume file events to not trigger get() update
        let mut buffer = [0u8; 1024];
        match self.inotify.read_events(&mut buffer) {
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(value),
            Err(err) => Err(err.into()),
            _ => Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brightness::test_support::BacklightFixture;

    #[test]
    fn replacement_and_removal_invalidate_open_backlight() {
        smol::block_on(async {
            for recreate in [false, true] {
                let fixture = BacklightFixture::new(255);
                let mut backend = Backlight::new(fixture.path.to_str().unwrap(), 1)
                    .await
                    .unwrap();
                backend.get().await.unwrap();
                if recreate {
                    fixture.replace(100);
                } else {
                    fixture.remove();
                }
                assert!(backend.get().await.unwrap_err().is::<BackendChanged>());
                assert!(backend.set(248).await.unwrap_err().is::<BackendChanged>());
                if recreate {
                    assert_eq!(
                        "50\n",
                        fs::read_to_string(fixture.path.join("brightness")).unwrap()
                    );
                    assert_eq!(
                        100,
                        Backlight::new(fixture.path.to_str().unwrap(), 1)
                            .await
                            .unwrap()
                            .max()
                    );
                }
            }
        });
    }

    #[test]
    fn maximum_changes_are_detected_without_inode_replacement() {
        smol::block_on(async {
            let fixture = BacklightFixture::new(255);
            let mut backend = Backlight::new(fixture.path.to_str().unwrap(), 1)
                .await
                .unwrap();
            fs::write(fixture.path.join("brightness"), "123\n").unwrap();
            assert_eq!(123, backend.get().await.unwrap());
            fs::write(fixture.path.join("max_brightness"), "100\n").unwrap();
            assert!(backend.validate().unwrap_err().is::<BackendChanged>());
            assert!(backend.set(99).await.is_err());
            assert_eq!(
                "123\n",
                fs::read_to_string(fixture.path.join("brightness")).unwrap()
            );
        });
    }

    #[test]
    fn class_symlink_retargeting_invalidates_identity() {
        smol::block_on(async {
            let fixture = BacklightFixture::new(255);
            let class = fixture.root.join("class");
            std::os::unix::fs::symlink(&fixture.path, &class).unwrap();
            let backend = Backlight::new(class.to_str().unwrap(), 1).await.unwrap();
            let alternate = fixture.root.join("alternate");
            fs::create_dir(&alternate).unwrap();
            fs::write(alternate.join("brightness"), "50\n").unwrap();
            fs::write(alternate.join("max_brightness"), "255\n").unwrap();
            let next = fixture.root.join("next");
            std::os::unix::fs::symlink(&alternate, &next).unwrap();
            fs::rename(next, class).unwrap();
            assert!(backend.validate().unwrap_err().is::<BackendChanged>());
        });
    }

    #[test]
    fn invalid_limits_fail_before_any_write() {
        smol::block_on(async {
            for (maximum, minimum) in [(0, 0), (100, 101)] {
                let fixture = BacklightFixture::new(maximum);
                let original = fs::read(fixture.path.join("brightness")).unwrap();
                assert!(Backlight::new(fixture.path.to_str().unwrap(), minimum)
                    .await
                    .is_err());
                assert_eq!(original, fs::read(fixture.path.join("brightness")).unwrap());
            }
        });
    }

    #[test]
    fn led_brightness_requires_polling() {
        assert!(requires_polling(Path::new(
            "/sys/class/leds/dell::kbd_backlight"
        )));
    }

    #[test]
    fn display_brightness_uses_inotify() {
        assert!(!requires_polling(Path::new(
            "/sys/class/backlight/intel_backlight"
        )));
    }

    #[test]
    fn pending_polled_write_hides_stale_brightness() {
        let now = Instant::now();
        let mut pending = Some((0, now + POLLED_WRITE_SETTLE_TIMEOUT));

        assert_eq!(0, settle_polled_write(&mut pending, 2, Some(0), now));
        assert!(pending.is_some());
        assert_eq!(0, settle_polled_write(&mut pending, 0, Some(0), now));
        assert!(pending.is_none());
    }

    #[test]
    fn pending_polled_write_expires() {
        let now = Instant::now();
        let mut pending = Some((0, now));

        assert_eq!(2, settle_polled_write(&mut pending, 2, Some(0), now));
        assert!(pending.is_none());
    }
}
