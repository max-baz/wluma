use super::ramp;
use anyhow::{anyhow, Context, Result};
use dbus::arg::PropMap;
use dbus::blocking::Connection;
use std::time::Duration;

const DESTINATION: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
const TIMEOUT: Duration = Duration::from_secs(5);

type Crtc = (u32, i64, i32, i32, i32, i32, i32, u32, Vec<u32>, PropMap);
type Output = (u32, i64, i32, Vec<u32>, String, Vec<u32>, Vec<u32>, PropMap);
type Mode = (u32, i64, u32, u32, f64, u32);
type Resources = (u32, Vec<Crtc>, Vec<Output>, Vec<Mode>, i32, i32);

pub struct Backend {
    connection: Connection,
    serial: u32,
    crtc: u32,
    original: [Vec<u16>; 3],
}

impl Backend {
    pub fn new(output_name: &str) -> Result<Self> {
        let connection = Connection::new_session().context("Unable to connect to session D-Bus")?;
        let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let (serial, _, outputs, _, _, _): Resources = proxy
            .method_call(INTERFACE, "GetResources", ())
            .context("Mutter DisplayConfig is unavailable")?;
        let output = find_output(&outputs, output_name)?;
        if output.2 < 0 {
            return Err(anyhow!("Mutter output '{output_name}' is inactive"));
        }
        let crtc = output.2 as u32;
        let (red, green, blue): (Vec<u16>, Vec<u16>, Vec<u16>) = proxy
            .method_call(INTERFACE, "GetCrtcGamma", (serial, crtc))
            .context("Unable to read Mutter gamma ramps")?;
        if red.is_empty() || red.len() != green.len() || red.len() != blue.len() {
            return Err(anyhow!("Mutter reported an invalid gamma LUT"));
        }
        log::debug!("Using Mutter DisplayConfig gamma control for '{output_name}'");
        Ok(Self {
            connection,
            serial,
            crtc,
            original: [red, green, blue],
        })
    }

    pub fn set(&mut self, dim: u64, temperature: u64) -> Result<()> {
        let [red, green, blue] = ramp::apply(&self.original, dim, temperature);
        let proxy = self.connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let _: () = proxy
            .method_call(
                INTERFACE,
                "SetCrtcGamma",
                (self.serial, self.crtc, red, green, blue),
            )
            .context("Unable to set Mutter gamma ramps")?;
        Ok(())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let proxy = self.connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let [red, green, blue] = self.original.clone();
        let _: Result<(), _> = proxy.method_call(
            INTERFACE,
            "SetCrtcGamma",
            (self.serial, self.crtc, red, green, blue),
        );
    }
}

fn find_output<'a>(outputs: &'a [Output], output_name: &str) -> Result<&'a Output> {
    if let Some(output) = outputs.iter().find(|output| output.4 == output_name) {
        return Ok(output);
    }

    let mut matches = outputs
        .iter()
        .filter(|output| output.4.contains(output_name));
    let Some(output) = matches.next() else {
        return Err(anyhow!(
            "Unable to match '{output_name}' to a Mutter output"
        ));
    };
    if matches.next().is_some() {
        return Err(anyhow!("Multiple Mutter outputs match '{output_name}'"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{find_output, Crtc, Mode, Output};
    use dbus::arg::Arg;

    fn output(name: &str) -> Output {
        (
            0,
            0,
            0,
            vec![],
            name.to_string(),
            vec![],
            vec![],
            Default::default(),
        )
    }

    #[test]
    fn resource_types_match_mutter_dbus_signatures() {
        assert_eq!(Crtc::signature().to_string(), "(uxiiiiiuaua{sv})");
        assert_eq!(Output::signature().to_string(), "(uxiausauaua{sv})");
        assert_eq!(Mode::signature().to_string(), "(uxuudu)");
    }

    #[test]
    fn matches_exact_output_before_partial_matches() {
        let outputs = [output("DP-1"), output("card0-DP-1")];

        assert_eq!(find_output(&outputs, "DP-1").unwrap().4, "DP-1");
    }

    #[test]
    fn matches_a_unique_partial_output_name() {
        let outputs = [output("card0-DP-1"), output("card0-HDMI-1")];

        assert_eq!(find_output(&outputs, "DP-1").unwrap().4, "card0-DP-1");
    }

    #[test]
    fn rejects_missing_or_ambiguous_output_names() {
        let outputs = [output("card0-DP-1"), output("card1-DP-1")];

        assert!(find_output(&outputs, "eDP-1").is_err());
        assert!(find_output(&outputs, "DP-1").is_err());
    }
}
