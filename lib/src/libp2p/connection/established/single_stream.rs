// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! State machine handling a single TCP or WebSocket libp2p connection.
//!
//! # About resources allocation and back-pressure
//!
//! In order to avoid DoS attacks, it is important, in networking code, to make sure that the
//! amount of memory allocated directly or indirectly by a connection stays bounded.
//!
//! The situations in the [`SingleStream`] that lead to an increase in memory consumption are:
//!
//! 1- On incoming or outgoing substreams.
//! 2- When sending a request or receiving a response in a request-response protocol.
//! 3- When sending a notification.
//! 4- When receiving a request and sending back a response.
//! 5- When receiving a notification.
//! // TODO: 6- on Yamux ping frames
//!
//! In order to solve 1-, there exists a maximum number of simultaneous substreams allowed by the
//! protocol, thereby guaranteeing that the memory consumption doesn't exceed a certain bound.
//! Since receiving a request and a response is a one-time process that occupies an entire
//! substream, allocations referenced by points 2- and 4- are also bounded thanks to this limit.
//! Request-response protocols enforce a limit to the size of the request and response, again
//! guaranteeing a bound on the memory consumption.
//!
//! In order to solve 3-, always use [`SingleStream::notification_substream_queued_bytes`] in order
//! to check the current amount of buffered data before calling
//! [`SingleStream::write_notification_unbounded`]. See the documentation of
//! [`SingleStream::write_notification_unbounded`] for more details.
//!
//! In order to solve 5-, // TODO: .
//!

// TODO: expand docs ^

// TODO: consider implementing on top of multi_stream

use super::{
    super::{super::read_write::ReadWrite, noise, yamux},
    substream::{self, RespondInRequestError},
    Config, Event, SubstreamId, SubstreamIdInner,
};

use alloc::{boxed::Box, string::String, vec::Vec};
use core::{
    fmt,
    num::{NonZeroU32, NonZeroUsize},
    ops::{Add, Index, IndexMut, Sub},
    time::Duration,
};
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

pub use substream::InboundTy;

/// State machine of a fully-established connection.
pub struct SingleStream<TNow, TSubUd> {
    /// Encryption layer applied directly on top of the incoming data and outgoing data.
    encryption: noise::Noise,

    /// Extra fields. Segregated in order to solve borrowing questions.
    inner: Box<Inner<TNow, TSubUd>>,
}

/// Extra fields. Segregated in order to solve borrowing questions.
struct Inner<TNow, TSubUd> {
    /// State of the various substreams of the connection.
    /// Consists in a collection of substreams, each of which holding a [`substream::Substream`]
    /// object, or `None` if the substream has been reset.
    /// Also includes, for each substream, a collection of buffers whose data is to be written
    /// out.
    yamux: yamux::Yamux<Option<(substream::Substream<TNow>, Option<TSubUd>, Vec<u8>)>>,

    /// If `Some`, contains a substream whose read buffer contains data.
    substream_to_process: Option<yamux::SubstreamId>,

    /// Substream in [`Inner::yamux`] used for outgoing pings.
    ///
    /// Because of the API of [`substream::Substream`] concerning pings, there is no need to
    /// handle situations where the substream fails to negotiate, as this is handled by making
    /// outgoing pings error. This substream is therefore constant.
    ///
    /// It is possible, however, that the remote resets the ping substream. In other words, this
    /// substream might not be found in [`Inner::yamux`]. When that happens, all outgoing pings
    /// are immediately considered as failed.
    outgoing_pings: yamux::SubstreamId,
    /// When to start the next ping attempt.
    next_ping: TNow,
    /// Source of randomness to generate ping payloads.
    ///
    /// Note that we use ChaCha20 because the rest of the code base also uses ChaCha20. This avoids
    /// unnecessary code being included in the binary and reduces the binary size.
    ping_payload_randomness: rand_chacha::ChaCha20Rng,

    /// See [`Config::max_inbound_substreams`].
    max_inbound_substreams: usize,
    /// See [`Config::max_protocol_name_len`].
    max_protocol_name_len: usize,
    /// See [`Config::ping_interval`].
    ping_interval: Duration,
    /// See [`Config::ping_timeout`].
    ping_timeout: Duration,
}

impl<TNow, TSubUd> SingleStream<TNow, TSubUd>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Reads data coming from the socket, updates the internal state machine, and writes data
    /// destined to the socket through the [`ReadWrite`].
    ///
    /// In order to avoid unnecessary memory allocations, only one [`Event`] is returned at a time.
    /// Consequently, this method returns as soon as an event is available, even if the buffers
    /// haven't finished being read. Call this method in a loop until the number of bytes read and
    /// written are both 0, and the returned [`Event`] is `None`.
    ///
    /// If an error is returned, the socket should be entirely shut down.
    // TODO: consider exposing an API more similar to the one of substream::Substream::read_write?
    pub fn read_write(
        mut self,
        read_write: &'_ mut ReadWrite<TNow>,
    ) -> Result<(SingleStream<TNow, TSubUd>, Option<Event<TSubUd>>), Error> {
        // First, update all the internal substreams.
        // This doesn't read data from `read_write`, but can potential write out data.
        for substream_id in self
            .inner
            .yamux
            .user_datas()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
        {
            let (call_again, event) =
                Self::process_substream(&mut self.inner, substream_id, read_write);
            if let Some(event) = event {
                return Ok((self, Some(event)));
            } else if call_again {
                read_write.wake_up_asap();
            }
        }

        // Start any outgoing ping if necessary.
        if read_write.now >= self.inner.next_ping {
            self.inner.next_ping = read_write.now.clone() + self.inner.ping_interval;

            // It might be that the remote has reset the ping substream, in which case the out ping
            // substream no longer exists and we immediately consider the ping as failed.
            if self.inner.yamux.has_substream(self.inner.outgoing_pings) {
                let mut payload = [0u8; 32];
                self.inner.ping_payload_randomness.fill_bytes(&mut payload);
                self.inner
                    .yamux
                    .user_data_mut(self.inner.outgoing_pings)
                    .as_mut()
                    .unwrap()
                    .0
                    .queue_ping(&payload, read_write.now.clone() + self.inner.ping_timeout);
            } else {
                return Ok((self, Some(Event::PingOutFailed)));
            }
        }
        read_write.wake_up_after(&self.inner.next_ping);

        // Processing incoming data might be blocked on emitting data or on removing dead
        // substreams, and processing incoming data might lead to more data to emit. The easiest
        // way to implement this is a single loop that does everything.
        loop {
            // If we have both sent and received a GoAway frame, that means that no new substream
            // can be opened. If in addition to this there is no substream in the connection,
            // then we can safely close it as a normal termination.
            // Note that, because we have no guarantee that the remote has received our GoAway
            // frame yet, it is possible to receive requests for new substreams even after having
            // sent the GoAway. Because we close the writing side, it is not possible to indicate
            // to the remote that these new substreams are denied. However, this is not a problem
            // as the remote interprets our GoAway frame as an automatic refusal of all its pending
            // substream requests.
            if self.inner.yamux.is_empty()
                && self.inner.yamux.goaway_sent()
                && self.inner.yamux.received_goaway().is_some()
            {
                read_write.close_write();
            }

            // Any meaningful activity within this loop can set this value to `true`. If this
            // value is still `false` at the end of the loop, we return from the function due to
            // having nothing more to do.
            let mut must_continue_looping = false;

            if let Some(substream_id) = self.inner.substream_to_process {
                // It might be that the substream has been closed in `process_substream`.
                if !self.inner.yamux.has_substream(substream_id) {
                    self.inner.substream_to_process = None;
                    continue;
                }

                let (call_again, event) =
                    Self::process_substream(&mut self.inner, substream_id, read_write);

                if !call_again {
                    self.inner.substream_to_process = None;
                }

                if let Some(event) = event {
                    return Ok((self, Some(event)));
                } else if call_again {
                    // Jump back to the beginning of the loop. We don't want to read more data
                    // until this specific substream's data has been processed.
                    continue;
                }
            }

            // Note that we treat the reading side being closed the same way as no data being
            // received. The fact that the remote has closed their writing side is no different
            // than them leaving their writing side open but no longer send any data at all.
            // The remote is free to close their writing side at any point if it judges that it
            // will no longer need to send anymore data.
            // Note, however, that in principle the remote should have sent a GoAway frame prior
            // to closing their writing side. But this is not something we check or really care
            // about.

            let mut decrypted_read_write = self
                .encryption
                .read_write(read_write)
                .map_err(Error::Noise)?;

            // Ask the Yamux state machine to decode the data in `self.decrypted_data_buffer`.
            debug_assert!(self.inner.substream_to_process.is_none());
            let yamux_decode = self
                .inner
                .yamux
                .incoming_data(&decrypted_read_write.incoming_buffer)
                .map_err(Error::Yamux)?;
            self.inner.yamux = yamux_decode.yamux;

            // If bytes_read is 0 and detail is None, then Yamux can't do anything more. On the
            // other hand, if bytes_read is != 0 or detail is Some, then Yamux might have more
            // things to do, and we must loop again.
            if !(yamux_decode.bytes_read == 0 && yamux_decode.detail.is_none()) {
                must_continue_looping = true;
            }

            // Analyze how Yamux has parsed the data.
            // This still contains references to the data in `self.encryption`.
            match yamux_decode.detail {
                None if yamux_decode.bytes_read == 0 => {}
                None => {
                    let _ = decrypted_read_write.incoming_bytes_take(yamux_decode.bytes_read);
                }

                Some(yamux::IncomingDataDetail::IncomingSubstream) => {
                    debug_assert!(!self.inner.yamux.goaway_queued_or_sent());

                    let _ = decrypted_read_write.incoming_bytes_take(yamux_decode.bytes_read);

                    // Receive a request from the remote for a new incoming substream.
                    // These requests are automatically accepted unless the total limit to the
                    // number of substreams has been reached.
                    // Note that `num_inbound()` counts substreams that have been closed but not
                    // yet removed from the state machine. This can affect the actual limit in a
                    // subtle way. At the time of writing of this comment the limit should be
                    // properly enforced, however it is not considered problematic if it weren't.
                    if self.inner.yamux.num_inbound() >= self.inner.max_inbound_substreams {
                        // Can only panic if there's no incoming substream, which we know for sure
                        // is the case here.
                        self.inner
                            .yamux
                            .reject_pending_substream()
                            .unwrap_or_else(|_| panic!());
                        continue;
                    }

                    // Can only panic if there's no incoming substream, which we know for sure
                    // is the case here.
                    self.inner
                        .yamux
                        .accept_pending_substream(Some((
                            substream::Substream::ingoing(self.inner.max_protocol_name_len),
                            None,
                            Vec::new(),
                        )))
                        .unwrap_or_else(|_| panic!());
                }

                Some(
                    yamux::IncomingDataDetail::StreamReset { .. }
                    | yamux::IncomingDataDetail::StreamClosed { .. },
                ) => {
                    let _ = decrypted_read_write.incoming_bytes_take(yamux_decode.bytes_read);
                }

                Some(yamux::IncomingDataDetail::DataFrame {
                    start_offset,
                    substream_id,
                }) => {
                    self.inner
                        .yamux
                        .user_data_mut(substream_id)
                        .as_mut()
                        .unwrap()
                        .2
                        .extend_from_slice(
                            &decrypted_read_write.incoming_buffer
                                [start_offset..yamux_decode.bytes_read],
                        );

                    let _ = decrypted_read_write.incoming_bytes_take(yamux_decode.bytes_read);
                    self.inner.substream_to_process = Some(substream_id);
                }

                Some(yamux::IncomingDataDetail::GoAway { .. }) => {
                    // TODO: somehow report the GoAway error code on the external API?
                    let _ = decrypted_read_write.incoming_bytes_take(yamux_decode.bytes_read);
                    drop(decrypted_read_write);
                    return Ok((self, Some(Event::NewOutboundSubstreamsForbidden)));
                }

                Some(yamux::IncomingDataDetail::PingResponse) => {
                    // Can only happen if we send out pings, which we never do.
                    unreachable!()
                }
            };

            // The yamux or encryption state machines might contain data that needs to be
            // written out. Try to flush them.
            // The API user is supposed to call `read_write` in a loop until the number of bytes
            // written out is 0, meaning that there's no need to set `must_continue_looping` to
            // `true`.
            while let Some(buffer) = self
                .inner
                .yamux
                .extract_next(decrypted_read_write.write_bytes_queueable.unwrap_or(0))
            {
                decrypted_read_write.write_out(buffer.as_ref().to_vec());
            }

            drop(decrypted_read_write);

            // Substreams that have been closed or reset aren't immediately removed the yamux state
            // machine. They must be removed manually, which is what is done here.
            let dead_substream_ids = self
                .inner
                .yamux
                .dead_substreams()
                .map(|(id, death_ty, _)| (id, death_ty))
                .collect::<Vec<_>>();
            for (dead_substream_id, death_ty) in dead_substream_ids {
                match death_ty {
                    yamux::DeadSubstreamTy::Reset => {
                        // If the substream has been reset, we simply remove it from the Yamux
                        // state machine.

                        // If the substream was reset by the remote, then the substream state
                        // machine will still be `Some`.
                        if let Some((state_machine, mut user_data, _)) =
                            self.inner.yamux.remove_dead_substream(dead_substream_id)
                        {
                            // TODO: consider changing this `state_machine.reset()` function to be a state transition of the substream state machine (that doesn't take ownership), to simplify the implementation of both the substream state machine and this code
                            if let Some(event) = state_machine.reset() {
                                return Ok((
                                    self,
                                    Some(Self::pass_through_substream_event(
                                        dead_substream_id,
                                        &mut user_data,
                                        event,
                                    )),
                                ));
                            }
                        };

                        // Removing a dead substream might lead to Yamux being able to process more
                        // incoming data. As such, we loop again.
                        must_continue_looping = true;
                    }
                    yamux::DeadSubstreamTy::ClosedGracefully => {
                        // If the substream has been closed gracefully, we don't necessarily
                        // remove it instantly. Instead, we continue processing the substream
                        // state machine until it tells us that there are no more events to
                        // return.

                        // Mutable reference to the substream state machine within the yamux
                        // state machine.
                        let state_machine_refmut =
                            self.inner.yamux.user_data_mut(dead_substream_id);

                        // Extract the substream state machine, maybe putting it back later.
                        let (
                            state_machine_extracted,
                            mut substream_user_data,
                            substream_read_buffer,
                        ) = match state_machine_refmut.take() {
                            Some(s) => s,
                            None => {
                                // Substream has already been removed from the Yamux state machine
                                // previously. We know that it can't yield any more event.
                                self.inner.yamux.remove_dead_substream(dead_substream_id);

                                // Removing a dead substream might lead to Yamux being able to
                                // process more incoming data. As such, we loop again.
                                must_continue_looping = true;

                                continue;
                            }
                        };

                        // Now we run `state_machine_extracted.read_write`.
                        let mut substream_read_write = ReadWrite {
                            now: read_write.now.clone(),
                            incoming_buffer: Vec::new(),
                            expected_incoming_bytes: None,
                            write_buffers: Vec::new(),
                            write_bytes_queued: 0,
                            write_bytes_queueable: None,
                            read_bytes: 0,
                            wake_up_after: None,
                        };

                        let (substream_update, event) =
                            state_machine_extracted.read_write(&mut substream_read_write);

                        debug_assert!(
                            substream_read_write.read_bytes == 0
                                && substream_read_write.write_bytes_queued == 0
                        );

                        if let Some(wake_up_after) = substream_read_write.wake_up_after {
                            read_write.wake_up_after(&wake_up_after);
                        }

                        let event_pass_through = event.map(|ev| {
                            Self::pass_through_substream_event(
                                dead_substream_id,
                                &mut substream_user_data,
                                ev,
                            )
                        });

                        if let Some(substream_update) = substream_update {
                            // Put back the substream state machine. It will be picked up again
                            // the next time `read_write` is called.
                            *state_machine_refmut = Some((
                                substream_update,
                                substream_user_data,
                                substream_read_buffer,
                            ));
                        } else {
                            // Substream has no more events to give us. Remove it from the Yamux
                            // state machine.
                            self.inner.yamux.remove_dead_substream(dead_substream_id);

                            // Removing a dead substream might lead to Yamux being able to process more
                            // incoming data. As such, we loop again.
                            must_continue_looping = true;
                        }

                        if let Some(event_pass_through) = event_pass_through {
                            return Ok((self, Some(event_pass_through)));
                        }
                    }
                }
            }

            // If `must_continue_looping` is still false, then we didn't do anything meaningful
            // during this iteration. Return due to idleness.
            if !must_continue_looping {
                return Ok((self, None));
            }
        }
    }

    /// Advances a single substream.
    ///
    /// Returns a `boolean` indicating whether the substream should be processed again as soon as
    /// possible. Also optionally returns an event to yield to the user.
    ///
    /// If the substream wants to wake up at a certain time or after a certain future,
    /// `outer_read_write` will be updated to also wake up at that moment.
    ///
    /// This function does **not** read incoming data from `outer_read_write`. Instead, the data
    /// destined to the substream is found in `in_data`.
    ///
    /// # Panic
    ///
    /// Panics if the substream has its read point closed and `in_data` isn't empty.
    ///
    fn process_substream(
        inner: &mut Inner<TNow, TSubUd>,
        substream_id: yamux::SubstreamId,
        outer_read_write: &mut ReadWrite<TNow>,
    ) -> (bool, Option<Event<TSubUd>>) {
        let (state_machine, mut substream_user_data, substream_read_buffer) =
            match inner.yamux.user_data_mut(substream_id).take() {
                Some(s) => s,
                None => return (false, None),
            };

        let read_is_closed = !inner.yamux.can_receive(substream_id);
        let write_is_closed = !inner.yamux.can_send(substream_id);

        let mut substream_read_write = ReadWrite {
            now: outer_read_write.now.clone(),
            expected_incoming_bytes: if !read_is_closed { Some(0) } else { None },
            incoming_buffer: substream_read_buffer,
            read_bytes: 0,
            write_buffers: Vec::new(),
            write_bytes_queued: 0,
            write_bytes_queueable: if write_is_closed {
                None
            } else {
                Some(usize::max_value())
            },
            wake_up_after: None,
        };

        let (substream_update, event) = state_machine.read_write(&mut substream_read_write);

        if let Some(wake_up_after) = substream_read_write.wake_up_after {
            outer_read_write.wake_up_after(&wake_up_after);
        }

        // Give the possibility for the remote to send more data.
        // TODO: only do that for notification substreams? because for requests we already set the value to the maximum when the substream is created
        inner.yamux.add_remote_window_saturating(
            substream_id,
            u64::try_from(substream_read_write.read_bytes).unwrap(),
        );

        let closed_after = substream_read_write.write_bytes_queueable.is_none();
        for buffer in substream_read_write.write_buffers {
            if buffer.is_empty() {
                continue;
            }
            debug_assert!(!write_is_closed);
            inner.yamux.write(substream_id, buffer).unwrap();
        }
        if !write_is_closed && closed_after {
            inner.yamux.close(substream_id).unwrap();
        }

        let event_to_yield = event.map(|ev| {
            Self::pass_through_substream_event(substream_id, &mut substream_user_data, ev)
        });

        match substream_update {
            Some(s) => {
                *inner.yamux.user_data_mut(substream_id) =
                    Some((s, substream_user_data, substream_read_write.incoming_buffer))
            }
            None => {
                if !closed_after || !read_is_closed {
                    // TODO: what we do here is definitely correct, but the docs of `reset()` seem sketchy, investigate
                    inner.yamux.reset(substream_id).unwrap();
                }
            }
        };

        let call_again = substream_read_write.read_bytes != 0
            || substream_read_write.write_bytes_queued != 0
            || event_to_yield.is_some();

        (call_again, event_to_yield)
    }

    /// Turns an event from the [`substream`] module into an [`Event`].
    fn pass_through_substream_event(
        substream_id: yamux::SubstreamId,
        substream_user_data: &mut Option<TSubUd>,
        event: substream::Event,
    ) -> Event<TSubUd> {
        match event {
            substream::Event::InboundError {
                error,
                was_accepted: false,
            } => Event::InboundError(error),
            substream::Event::InboundError {
                was_accepted: true, ..
            } => Event::InboundAcceptedCancel {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                user_data: substream_user_data.take().unwrap(),
                // TODO: notify of the error?
            },
            substream::Event::InboundNegotiated(protocol_name) => Event::InboundNegotiated {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                protocol_name,
            },
            substream::Event::InboundNegotiatedCancel => Event::InboundNegotiatedCancel {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
            },
            substream::Event::RequestIn { request } => Event::RequestIn {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                request,
            },
            substream::Event::Response { response } => Event::Response {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                response,
                user_data: substream_user_data.take().unwrap(),
            },
            substream::Event::NotificationsInOpen { handshake } => Event::NotificationsInOpen {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                handshake,
            },
            substream::Event::NotificationsInOpenCancel => Event::NotificationsInOpenCancel {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
            },
            substream::Event::NotificationIn { notification } => Event::NotificationIn {
                notification,
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
            },
            substream::Event::NotificationsInClose { outcome } => Event::NotificationsInClose {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                outcome,
                user_data: substream_user_data.take().unwrap(),
            },
            substream::Event::NotificationsOutResult { result } => Event::NotificationsOutResult {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                result: match result {
                    Ok(r) => Ok(r),
                    Err(err) => Err((err, substream_user_data.take().unwrap())),
                },
            },
            substream::Event::NotificationsOutCloseDemanded => {
                Event::NotificationsOutCloseDemanded {
                    id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                }
            }
            substream::Event::NotificationsOutReset => Event::NotificationsOutReset {
                id: SubstreamId(SubstreamIdInner::SingleStream(substream_id)),
                user_data: substream_user_data.take().unwrap(),
            },
            substream::Event::PingOutSuccess => Event::PingOutSuccess,
            substream::Event::PingOutError { .. } => {
                // Because ping events are automatically generated by the external API without any
                // guarantee, it is safe to merge multiple failed pings into one.
                Event::PingOutFailed
            }
        }
    }

    /// Close the incoming substreams, automatically denying any new substream request from the
    /// remote.
    ///
    /// Note that this does not prevent incoming-substreams-related events
    /// (such as [`Event::RequestIn`]) from being generated, as it is possible that the remote has
    /// already opened a substream but has no sent all the necessary handshake messages yet.
    ///
    /// # Panic
    ///
    /// Panic if this function has been called before. It is illegal to call
    /// [`SingleStream::deny_new_incoming_substreams`] more than one on the same connections.
    ///
    pub fn deny_new_incoming_substreams(&mut self) {
        // TODO: arbitrary yamux error code
        self.inner
            .yamux
            .send_goaway(yamux::GoAwayErrorCode::NormalTermination)
            .unwrap()
    }

    /// Sends a request to the remote.
    ///
    /// This method only inserts the request into the connection object. Use
    /// [`SingleStream::read_write`] in order to actually send out the request.
    ///
    /// Assuming that the remote is using the same implementation, an [`Event::RequestIn`] will
    /// be generated on its side.
    ///
    /// If `request` is `None`, then no request is sent to the remote at all. If `request` is
    /// `Some`, then a (potentially-empty) request is sent. If `Some(&[])` is provided, a
    /// length-prefix containing a 0 is sent to the remote.
    ///
    /// After the remote has sent back a response, an [`Event::Response`] event will be generated
    /// locally. The `user_data` parameter will be passed back.
    ///
    /// The timeout is the time between the moment the substream is opened and the moment the
    /// response is sent back. If the emitter doesn't send the request or if the receiver doesn't
    /// answer during this time window, the request is considered failed.
    ///
    /// # Panic
    ///
    /// Panics if a [`Event::NewOutboundSubstreamsForbidden`] event has been generated in the past.
    ///
    pub fn add_request(
        &mut self,
        protocol_name: String,
        request: Option<Vec<u8>>,
        timeout: TNow,
        max_response_size: usize,
        user_data: TSubUd,
    ) -> SubstreamId {
        let substream_id = self
            .inner
            .yamux
            .open_substream(Some((
                substream::Substream::request_out(
                    protocol_name,
                    timeout,
                    request,
                    max_response_size,
                ),
                Some(user_data),
                Vec::new(),
            )))
            .unwrap(); // TODO: consider not panicking

        // TODO: we add some bytes due to the length prefix, this is a bit hacky as we should ask this information from the substream
        self.inner.yamux.add_remote_window_saturating(
            substream_id,
            u64::try_from(max_response_size)
                .unwrap_or(u64::max_value())
                .saturating_add(64)
                .saturating_sub(yamux::NEW_SUBSTREAMS_FRAME_SIZE),
        );

        SubstreamId(SubstreamIdInner::SingleStream(substream_id))
    }

    /// Opens a outgoing substream with the given protocol, destined for a stream of
    /// notifications.
    ///
    /// The remote must first accept (or reject) the substream before notifications can be sent
    /// on it.
    ///
    /// This method only inserts the opening handshake into the connection object. Use
    /// [`SingleStream::read_write`] in order to actually send out the request.
    ///
    /// Assuming that the remote is using the same implementation, an
    /// [`Event::NotificationsInOpen`] will be generated on its side.
    ///
    /// # Panic
    ///
    /// Panics if a [`Event::NewOutboundSubstreamsForbidden`] event has been generated in the past.
    ///
    pub fn open_notifications_substream(
        &mut self,
        protocol_name: String,
        handshake: Vec<u8>,
        max_handshake_size: usize,
        timeout: TNow,
        user_data: TSubUd,
    ) -> SubstreamId {
        let substream = self
            .inner
            .yamux
            .open_substream(Some((
                substream::Substream::notifications_out(
                    timeout,
                    protocol_name,
                    handshake,
                    max_handshake_size,
                ),
                Some(user_data),
                Vec::new(),
            )))
            .unwrap(); // TODO: consider not panicking

        SubstreamId(SubstreamIdInner::SingleStream(substream))
    }

    /// Call after an [`Event::InboundNegotiated`] has been emitted in order to accept the protocol
    /// name and indicate the type of the protocol.
    ///
    /// # Panic
    ///
    /// Panics if the substream is not in the correct state.
    ///
    pub fn accept_inbound(&mut self, substream_id: SubstreamId, ty: InboundTy, user_data: TSubUd) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        let (substream, ud, _) = self
            .inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap();
        substream.accept_inbound(ty);
        debug_assert!(ud.is_none());
        *ud = Some(user_data);
    }

    /// Call after an [`Event::InboundNegotiated`] has been emitted in order to reject the
    /// protocol name as not supported.
    ///
    /// # Panic
    ///
    /// Panics if the substream is not in the correct state.
    ///
    pub fn reject_inbound(&mut self, substream_id: SubstreamId) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        let (substream, ud, _) = self
            .inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap();
        substream.reject_inbound();
        debug_assert!(ud.is_none());
    }

    /// Accepts an inbound notifications protocol. Must be called in response to a
    /// [`Event::NotificationsInOpen`].
    ///
    /// # Panic
    ///
    /// Panics if the substream id is not valid or the substream is of the wrong type.
    ///
    pub fn accept_in_notifications_substream(
        &mut self,
        substream_id: SubstreamId,
        handshake: Vec<u8>,
        max_notification_size: usize,
    ) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .0
            .accept_in_notifications_substream(handshake, max_notification_size);
    }

    /// Rejects an inbound notifications protocol. Must be called in response to a
    /// [`Event::NotificationsInOpen`].
    ///
    /// # Panic
    ///
    /// Panics if the substream id is not valid or the substream is of the wrong type.
    ///
    pub fn reject_in_notifications_substream(&mut self, substream_id: SubstreamId) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .0
            .reject_in_notifications_substream();
    }

    /// Queues a notification to be written out on the given substream.
    ///
    /// # About back-pressure
    ///
    /// This method unconditionally queues up data. You must be aware that the remote, however,
    /// can decide to delay indefinitely the sending of that data, which can potentially lead to
    /// an unbounded increase in memory.
    ///
    /// As such, you are encouraged to call this method only if the amount of queued data (as
    /// determined by calling [`SingleStream::notification_substream_queued_bytes`]) is below a
    /// certain threshold. If above, the notification should be silently discarded.
    ///
    /// # Panic
    ///
    /// Panics if the [`SubstreamId`] doesn't correspond to a notifications substream, or if the
    /// notifications substream isn't in the appropriate state.
    ///
    pub fn write_notification_unbounded(
        &mut self,
        substream_id: SubstreamId,
        notification: Vec<u8>,
    ) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .0
            .write_notification_unbounded(notification);
    }

    /// Returns the number of bytes waiting to be sent out on that substream.
    ///
    /// See the documentation of [`SingleStream::write_notification_unbounded`] for context.
    ///
    /// # Panic
    ///
    /// Panics if the [`SubstreamId`] doesn't correspond to a notifications substream, or if the
    /// notifications substream isn't in the appropriate state.
    ///
    pub fn notification_substream_queued_bytes(&self, substream_id: SubstreamId) -> usize {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        let already_queued = self.inner.yamux.queued_bytes(substream_id);
        let from_substream = self
            .inner
            .yamux
            .user_data(substream_id)
            .as_ref()
            .unwrap()
            .0
            .notification_substream_queued_bytes();
        already_queued + from_substream
    }

    /// Closes a notifications substream opened after a successful
    /// [`Event::NotificationsOutResult`] or that was accepted using
    /// [`SingleStream::accept_in_notifications_substream`].
    ///
    /// In the case of an outbound substream, this can be done even when in the negotiation phase,
    /// in other words before the remote has accepted/refused the substream.
    ///
    /// # Panic
    ///
    /// Panics if the [`SubstreamId`] doesn't correspond to a notifications substream, or if the
    /// notifications substream isn't in the appropriate state.
    ///
    pub fn close_notifications_substream(&mut self, substream_id: SubstreamId) {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        if !self.inner.yamux.has_substream(substream_id) {
            panic!()
        }

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .0
            .close_notifications_substream();
    }

    /// Responds to an incoming request. Must be called in response to a [`Event::RequestIn`].
    ///
    /// Passing an `Err` corresponds, on the other side, to a
    /// [`substream::RequestError::SubstreamClosed`].
    ///
    /// Returns an error if the [`SubstreamId`] is invalid.
    pub fn respond_in_request(
        &mut self,
        substream_id: SubstreamId,
        response: Result<Vec<u8>, ()>,
    ) -> Result<(), RespondInRequestError> {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => return Err(RespondInRequestError::SubstreamClosed),
        };

        if !self.inner.yamux.has_substream(substream_id) {
            return Err(RespondInRequestError::SubstreamClosed);
        }

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .0
            .respond_in_request(response)
    }
}

impl<TNow, TSubUd> Index<SubstreamId> for SingleStream<TNow, TSubUd> {
    type Output = TSubUd;

    fn index(&self, substream_id: SubstreamId) -> &Self::Output {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        self.inner
            .yamux
            .user_data(substream_id)
            .as_ref()
            .unwrap()
            .1
            .as_ref()
            .unwrap()
    }
}

impl<TNow, TSubUd> IndexMut<SubstreamId> for SingleStream<TNow, TSubUd> {
    fn index_mut(&mut self, substream_id: SubstreamId) -> &mut Self::Output {
        let substream_id = match substream_id.0 {
            SubstreamIdInner::SingleStream(id) => id,
            _ => panic!(),
        };

        self.inner
            .yamux
            .user_data_mut(substream_id)
            .as_mut()
            .unwrap()
            .1
            .as_mut()
            .unwrap()
    }
}

impl<TNow, TSubUd> fmt::Debug for SingleStream<TNow, TSubUd>
where
    TSubUd: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_map()
            .entries(self.inner.yamux.user_datas())
            .finish()
    }
}

/// Error during a connection. The connection should be shut down.
#[derive(Debug, derive_more::Display)]
pub enum Error {
    /// Error in the noise cipher. Data has most likely been corrupted.
    #[display(fmt = "Noise error: {_0}")]
    Noise(noise::CipherError),
    /// Error while encoding noise data.
    #[display(fmt = "{_0}")]
    NoiseEncrypt(noise::EncryptError),
    /// Error in the Yamux multiplexing protocol.
    #[display(fmt = "Yamux error: {_0}")]
    Yamux(yamux::Error),
}

/// Successfully negotiated connection. Ready to be turned into a [`SingleStream`].
pub struct ConnectionPrototype {
    encryption: noise::Noise,
}

impl ConnectionPrototype {
    /// Builds a new [`ConnectionPrototype`] of a connection using the Noise and Yamux protocols.
    pub(crate) fn from_noise_yamux(encryption: noise::Noise) -> Self {
        ConnectionPrototype { encryption }
    }

    /// Extracts the Noise state machine from this prototype.
    pub fn into_noise_state_machine(self) -> noise::Noise {
        self.encryption
    }

    /// Turns this prototype into an actual connection.
    pub fn into_connection<TNow, TSubUd>(self, config: Config<TNow>) -> SingleStream<TNow, TSubUd>
    where
        TNow: Clone + Ord,
    {
        let mut randomness = rand_chacha::ChaCha20Rng::from_seed(config.randomness_seed);

        let mut yamux = yamux::Yamux::new(yamux::Config {
            is_initiator: self.encryption.is_initiator(),
            capacity: config.substreams_capacity,
            randomness_seed: {
                let mut seed = [0; 32];
                randomness.fill_bytes(&mut seed);
                seed
            },
            max_out_data_frame_size: NonZeroU32::new(8192).unwrap(), // TODO: make configurable?
            max_simultaneous_queued_pongs: NonZeroUsize::new(4).unwrap(),
            max_simultaneous_rst_substreams: NonZeroUsize::new(1024).unwrap(),
        });

        let outgoing_pings = yamux
            .open_substream(Some((
                substream::Substream::ping_out(config.ping_protocol.clone()),
                None,
                Vec::new(),
            )))
            // Can only panic if a `GoAway` has been received, or if there are too many substreams
            // already open, which we know for sure can't happen here
            .unwrap_or_else(|_| panic!());

        SingleStream {
            encryption: self.encryption,
            inner: Box::new(Inner {
                yamux,
                substream_to_process: None,
                outgoing_pings,
                next_ping: config.first_out_ping,
                ping_payload_randomness: randomness,
                max_inbound_substreams: config.max_inbound_substreams,
                max_protocol_name_len: config.max_protocol_name_len,
                ping_interval: config.ping_interval,
                ping_timeout: config.ping_timeout,
            }),
        }
    }
}

impl fmt::Debug for ConnectionPrototype {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ConnectionPrototype").finish()
    }
}
