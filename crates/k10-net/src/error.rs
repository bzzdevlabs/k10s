#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The port (or address and port) is taken in that view.
    AddrInUse,
    /// The local address is not one of the interface's.
    AddrNotAvailable,
    /// No ephemeral port is free for that destination.
    NoPorts,
    /// A memory or socket quota refused the allocation.
    NoMemory,
    /// A fixed-size table (connections, listeners, sockets) is full.
    TableFull,
    /// The operation does not apply in the socket's state.
    InvalidState,
    /// Unknown or stale handle or listener.
    NotFound,
    /// No route or address for the destination.
    Unreachable,
    /// The peer refused the connection (RST to our SYN).
    Refused,
    /// The peer reset the connection.
    Reset,
    TimedOut,
    /// Nothing to read or no room to write now.
    WouldBlock,
    /// The connection is closed for this direction.
    Closed,
    /// A datagram larger than the path allows.
    TooLong,
    Invalid,
    /// The handle belongs to another instance (queue).
    WrongQueue,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}
