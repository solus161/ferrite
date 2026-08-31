/// Sent from the UI thread to the controller thread, which owns the librtlsdr
/// handle. Only settings the *device* has to be told about travel this way;
/// anything the DSP applies itself (volume, mute, de-emphasis) is read off a
/// shared atomic instead, so nothing on the audio path has to drain a channel.
pub enum CtrlSignal {
    CenterHz(u32),
    /// Channel bandwidth, not the tuner's IF filter — the controller widens it
    /// to clear the offset-tuning gap, the same way the initial setup does.
    Bandwidth(u32),
    /// Tenths of a dB, already snapped to a value the tuner's own table offers.
    ///
    /// Implies manual gain mode: on this hardware "set a gain" and "stop using
    /// AGC" are one action, and splitting them would leave the tuner in auto
    /// while the UI shows a fixed number.
    GainTenths(i32),
    /// Hand the RTL2832's digital AGC control of the level. Leaving AGC is
    /// [`GainTenths`](CtrlSignal::GainTenths), which restores a known gain
    /// rather than freezing whatever the AGC last landed on.
    AgcOn,
    /// Signed: most dongles need a negative correction.
    Ppm(i32),
    Quit,
}
