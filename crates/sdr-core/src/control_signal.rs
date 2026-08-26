/// This is the signal to be sent accross threat. For example:
/// - change center frequency, sample rate
/// - gain, mode
pub enum CtrlSignal {
    CenterHz(u32),
    Bandwidth(u32),
    /// Whole dB. The controller thread converts to the tenths librtlsdr wants.
    Gain(u32),
    Ppm(u32),
    Quit,
}
