use std::io;
use std::result;

use mqtt3;
use tokio_timer::TimerError;

pub type Result<T> = result::Result<T, Error>;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        Mqtt3(err: mqtt3::Error) {
            from()
            display("mqtt3 error: {}", err)
            description("Mqtt3 error {}")
            cause(err)
        }
        Timer(err: TimerError) {
            from()
            description("Timer error")
            cause(err)
            display("timer error: {}", err)
        }
        Other
    }
}
