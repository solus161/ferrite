/// Sent from the UI thread to the controller thread, which owns the librtlsdr
/// handle. Only settings the *device* has to be told about travel this way;
/// anything the DSP applies itself (volume, mute, de-emphasis) is read off a
/// shared atomic instead, so nothing on the audio path has to drain a channel.
pub enum CtrlSignal {
    /// The hardware LO, programmed into the dongle verbatim. This is the centre
    /// of the sampled span and the centre of the waterfall, not the channel
    /// being demodulated — that is [`TunedHz`](CtrlSignal::TunedHz).
    CenterHz(u32),
    /// The channel the frequency translator brings down to DC, absolute.
    ///
    /// The controller does not touch the device for this. The `Xlator` lives
    /// inside the SDR read callback on another thread, so the controller only
    /// converts this to an offset from the current centre and stores it in the
    /// shared atomic the callback polls once per USB buffer.
    ///
    /// The only signal that changes the *offset*. A
    /// [`CenterHz`](CtrlSignal::CenterHz) carries the channel along with it, so
    /// the difference stays put and the translator is left alone even though
    /// the frequency being demodulated has moved.
    TunedHz(u32),
    /// Channel bandwidth, not the tuner's IF filter — the controller widens it
    /// to clear the tunable span, the same way the initial setup does.
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
