use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub type Conn = Framed<UnixStream, LengthDelimitedCodec>;

pub fn framed(stream: UnixStream) -> Conn {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .new_framed(stream)
}
