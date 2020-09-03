use derive_more::Display;
use std::{collections::VecDeque, error::Error};

use crate::{
    common::client::ClientId,
    coordinator::{models::HeartBeatResponse, settings::FederatedLearningSettings},
};

#[derive(Eq, Debug, PartialEq, Default, Copy, Clone, Display)]
#[display(
    fmt = "Counters(waiting={} selected={} done={} done_and_inactive={} ignored={})",
    waiting,
    selected,
    done,
    done_and_inactive,
    ignored
)]
pub struct Counters {
    /// Number of active clients waiting for being selected. These
    /// clients are in the [`ClientState::Waiting`] state.
    pub waiting: u32,
    /// Number of active client selected to take part to the current
    /// training round. These clients are in the
    /// [`ClientState::Selected`] state
    pub selected: u32,
    /// Number of client selected to take part to the current training
    /// round that already finishe training.
    pub done: u32,
    pub done_and_inactive: u32,
    pub ignored: u32,
}

impl Counters {
    pub fn new() -> Self {
        Default::default()
    }
}

/// The state machine.
pub struct Protocol {
    counters: Counters,

    /// Whether all the round of training are done
    pub is_training_complete: bool,

    /// Coordinator configuration
    settings: FederatedLearningSettings,

    /// Current training round
    current_round: u32,

    /// Events emitted by the state machine
    events: VecDeque<Event>,

    waiting_for_aggregation: bool,
}

impl Protocol {
    fn number_of_clients_to_select(&self) -> Option<u32> {
        if self.is_training_complete || self.waiting_for_aggregation {
            return None;
        }

        let Counters {
            waiting,
            selected,
            done,
            done_and_inactive,
            ..
        } = self.counters;

        let total_participants = selected + done + done_and_inactive;
        if total_participants >= self.settings.minimum_participants() {
            return None;
        }

        // We need to select more clients. But do we have enough
        // clients to perform the selection?
        let total_clients = total_participants + waiting;
        if total_clients < self.settings.min_clients {
            return None;
        }

        let total_to_select =
            f64::ceil(self.settings.participants_ratio * total_clients as f64) as i64 as u32;
        Some(total_to_select - total_participants)
    }

    fn maybe_start_selection(&mut self) {
        debug!(counters = %self.counters, "checking is more participants should be selected");
        if let Some(count) = self.number_of_clients_to_select() {
            info!(counters = %self.counters, "selecting {} additional participants", count);
            self.emit_event(Event::RunSelection(count))
        }
    }

    fn is_end_of_round(&self) -> bool {
        self.counters.selected == 0 && self.number_of_clients_to_select().is_none()
    }

    /// Emit an event
    fn emit_event(&mut self, event: Event) {
        self.events.push_back(event);
    }
}

// public methods
impl Protocol {
    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn new(settings: FederatedLearningSettings) -> Self {
        Self {
            settings,
            counters: Counters::new(),
            is_training_complete: false,
            waiting_for_aggregation: false,
            current_round: 0,
            events: VecDeque::new(),
        }
    }
    pub fn select(&mut self, mut candidates: impl Iterator<Item = (ClientId, ClientState)>) {
        debug!("processing candidates for selection");
        if let Some(mut total_needed) = self.number_of_clients_to_select() {
            while total_needed > 0 {
                match candidates.next() {
                    Some((id, ClientState::Waiting)) => {
                        debug!("selecting candidate {}", id);
                        self.counters.selected += 1;
                        self.counters.waiting -= 1;
                        total_needed -= 1;
                        self.emit_event(Event::SetState(id, ClientState::Selected));
                    }
                    Some((id, _)) => {
                        debug!("discarding candidate {}", id);
                    }
                    None => {
                        break;
                    }
                }
            }
        }
        self.maybe_start_selection();
    }

    /// Handle a rendez-vous request for the given client.
    ///
    /// # Returns
    ///
    /// This method returns the response to send back to the client.
    pub fn rendez_vous(&mut self, id: ClientId, client_state: ClientState) -> RendezVousResponse {
        info!("rendez vous: {}({})", id, client_state);
        if self.is_training_complete {
            return RendezVousResponse::Reject;
        }
        let response = match client_state {
            ClientState::Unknown => {
                // Accept new clients and make them selectable
                self.counters.waiting += 1;
                self.emit_event(Event::Accept(id));
                RendezVousResponse::Accept
            }
            ClientState::Waiting => {
                // The client should not re-send a rendez-vous
                // request, but that can be the case if it got
                // re-started so let's accept the client again.
                RendezVousResponse::Accept
            }
            ClientState::Selected => {
                // A selected/training client should not send us
                // a rendez-vous request. Let's not rely on it
                // for that round but still accept it for the
                // next round. The idea is to mitigate attacks
                // when many clients connect to the coordinator
                // and drop out once selected, while not
                // penalizing honest clients that had a
                // connectivity issue.
                self.counters.selected -= 1;
                self.counters.ignored += 1;
                self.emit_event(Event::SetState(id, ClientState::Ignored));
                RendezVousResponse::Accept
            }
            ClientState::DoneAndInactive | ClientState::Done => {
                // A client that has finished training may send
                // us a rendez-vous request if it is
                // restarted. This is problematic because we
                // cannot put them back in the "Waiting"
                // state, otherwise they might be selected
                // again for the current training round, to
                // which they already participated. Therefore,
                // we accept these clients but mark them as
                // "Ignored", to exclude them from the
                // selection process.
                self.counters.ignored += 1;
                self.emit_event(Event::SetState(id, ClientState::Ignored));
                RendezVousResponse::Accept
            }
            ClientState::Ignored => RendezVousResponse::Accept,
        };
        self.maybe_start_selection();
        response
    }

    /// Handle a heartbeat timeout for the given client.
    pub fn heartbeat_timeout(&mut self, id: ClientId, client_state: ClientState) {
        info!("heartbeat timeout: {}({})", id, client_state);
        self.emit_event(Event::Remove(id));
        match client_state {
            ClientState::Selected => self.counters.selected -= 1,
            ClientState::Waiting => self.counters.waiting -= 1,
            ClientState::Unknown => {
                panic!("Unknown client {} does not have a heartbeat", id);
            }
            ClientState::DoneAndInactive => {
                panic!("Done and inactive client {} does not have a heartbeat", id);
            }
            ClientState::Done => {
                self.emit_event(Event::SetState(id, ClientState::DoneAndInactive));
                self.counters.done_and_inactive += 1;
            }
            ClientState::Ignored => {
                self.counters.ignored -= 1;
            }
        }
        self.maybe_start_selection();
    }

    /// Handle a heartbeat for the given client.
    ///
    /// # Returns
    ///
    /// This method returns the response to send back to the client.
    pub fn heartbeat(&mut self, id: ClientId, client_state: ClientState) -> HeartBeatResponse {
        info!("heartbeat: {}({})", id, client_state);
        if self.is_training_complete {
            self.emit_event(Event::ResetHeartBeat(id));
            return HeartBeatResponse::Finish;
        }
        match client_state {
            // Reject any client we don't know about. They must first
            // send a rendez-vous request to be recognized by the
            // coordinator.
            ClientState::Unknown => HeartBeatResponse::Reject,

            // The client may have come back to life. But once a
            // client has become inactive, it has to send a new
            // rendez-vous request and be accepted by the coordinator,
            // so we reject this heartbeat.
            ClientState::DoneAndInactive => HeartBeatResponse::Reject,

            // Client that are waiting or done should stand by
            ClientState::Ignored | ClientState::Waiting | ClientState::Done => {
                self.emit_event(Event::ResetHeartBeat(id));
                HeartBeatResponse::StandBy
            }

            // If the client has been selected, notify them.
            ClientState::Selected => {
                self.emit_event(Event::ResetHeartBeat(id));
                HeartBeatResponse::Round(self.current_round)
            }
        }
    }

    /// Handle a start training request for the given client.
    ///
    /// # Returns
    ///
    /// This method returns the response to send back to the client.
    pub fn start_training(&mut self, client_state: ClientState) -> StartTrainingResponse {
        if client_state == ClientState::Selected && !self.is_training_complete {
            info!("accepting start training request");
            StartTrainingResponse::Accept
        } else {
            info!(
                "rejecting start training request (client state = {}, training_complete = {}",
                client_state, self.is_training_complete
            );
            StartTrainingResponse::Reject
        }
    }

    /// Handle an end training request for the given client.
    ///
    /// # Returns
    ///
    /// This method returns the response to send back to the client.
    pub fn end_training(&mut self, id: ClientId, success: bool, client_state: ClientState) {
        info!(
            "end training request: {}({}) (success={})",
            id, client_state, success
        );
        if self.is_training_complete || self.waiting_for_aggregation {
            warn!("got unexpected end training request");
            return;
        }

        if client_state == ClientState::Selected {
            self.counters.selected -= 1;
            if success {
                self.emit_event(Event::SetState(id, ClientState::Done));
                self.counters.done += 1;

                if self.is_end_of_round() {
                    self.emit_event(Event::RunAggregation);
                    self.waiting_for_aggregation = true;
                    info!(
                        counters = %self.counters,
                        "round complete, resetting the clients"
                    );
                    self.emit_event(Event::ResetAll);
                    self.counters.waiting += self.counters.done;
                    self.counters.waiting += self.counters.ignored;
                    self.counters.done_and_inactive = 0;
                    self.counters.done = 0;
                    self.counters.ignored = 0;
                }
            } else {
                self.emit_event(Event::SetState(id, ClientState::Ignored));
                self.counters.ignored += 1;
                info!(counters = %self.counters, "training failed, ignoring participant");
            }
            self.maybe_start_selection();
        }
    }

    pub fn end_aggregation(&mut self, success: bool) {
        if !self.waiting_for_aggregation {
            error!("not waiting for aggregation");
            return;
        }
        self.waiting_for_aggregation = false;
        if success {
            self.emit_event(Event::EndRound(self.current_round));
            self.current_round += 1;
        }
        if self.current_round == self.settings.rounds {
            info!("training complete");
            self.is_training_complete = true;
        } else {
            info!("aggregation finished, proceeding to start a new round");
            self.maybe_start_selection();
        }
    }

    /// Retrieve the next event
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

impl FederatedLearningSettings {
    fn minimum_participants(&self) -> u32 {
        (self.participants_ratio * self.min_clients as f64) as i64 as u32
    }
}

/// Response to a "start training" request.
#[derive(Debug, PartialEq, Eq)]
pub enum StartTrainingResponse {
    Reject,
    Accept,
}

/// Response to a rendez-vous request
#[derive(Debug, PartialEq, Eq)]
pub enum RendezVousResponse {
    /// The coordinator accepts the client
    Accept,

    /// The coordinator rejects the client
    Reject,
}

/// Represent the state of a client, as seen by the state machine
#[derive(Eq, PartialEq, Hash, Debug, Copy, Clone, Display)]
pub enum ClientState {
    /// The client has not sent a rendez-vous request yet
    Unknown,
    /// The client has sent a rendez-vous request but has not been
    /// selected for a training round
    Waiting,
    /// The client has been selected for the current training round but
    /// hasn't started training yet
    Selected,
    // /// The client has been selected for the current training round and
    // /// has started training
    // Training,
    /// The client has been selected for the current training round and
    /// has finished training
    Done,
    /// The client has been selected for the current training round and
    /// has finished training but disconnected
    DoneAndInactive,
    /// The client is alive but excluded from the selection
    Ignored,
}

/// Events emitted by the state machine
#[derive(Debug, Eq, PartialEq)]
pub enum Event {
    /// Accept the given client. This client becomes selectable, _ie_
    /// has state [`ClientState::Waiting`].
    Accept(ClientId),

    /// Remove a client. This client becomes unknown [`ClientState::Unknown`].
    Remove(ClientId),

    /// Update the given client's state.
    SetState(ClientId, ClientState),

    /// Reset all the active clients to their default state:
    /// [`ClientState::Waiting`], and remove the inactive clients.
    ResetAll,

    /// Reset the heartbeat timer for the given client
    ResetHeartBeat(ClientId),

    /// Start the aggregation process
    RunAggregation,

    /// Start the selection process
    RunSelection(u32),

    /// Indicates the end of a round
    EndRound(u32),
}

#[derive(Debug, Display)]
pub struct InvalidState;
impl Error for InvalidState {}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{common::client::ClientId, coordinator::settings::FederatedLearningSettings};

    fn get_default_fl_settings() -> FederatedLearningSettings {
        FederatedLearningSettings {
            rounds: 2,
            participants_ratio: 1.0,
            min_clients: 1,
            heartbeat_timeout: 15,
        }
    }

    #[test]
    fn test_new() {
        let mut protocol = Protocol::new(get_default_fl_settings());

        let counters = protocol.counters();
        let expected = Counters {
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of single rendez-vous request
    #[test]
    fn test_rendez_vous_new_client() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.rendez_vous(client_id, ClientState::Unknown);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(RendezVousResponse::Accept, resp);
        assert_eq!(protocol.next_event().unwrap(), Event::Accept(client_id));
        assert_eq!(protocol.next_event().unwrap(), Event::RunSelection(1));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a rendez-vous request from a client that
    /// already sent a rendez-vous request but has not yet been selected
    #[test]
    fn test_rendez_vous_waiting_client_re_send_rendez_vous() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        protocol.rendez_vous(client_id, ClientState::Unknown);

        assert_eq!(1, protocol.counters().waiting);

        let resp = protocol.rendez_vous(client_id, ClientState::Waiting);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(RendezVousResponse::Accept, resp);
    }

    /// Test the outcome of a rendez-vous request from a client that
    /// already sent a rendez-vous request and has already been
    /// selected
    #[test]
    fn test_rendez_vous_selected_client_re_send_rendez_vous() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        protocol.rendez_vous(client_id, ClientState::Unknown);

        assert_eq!(1, protocol.counters().waiting);
        assert_eq!(protocol.next_event().unwrap(), Event::Accept(client_id));

        let candidates = vec![(client_id, ClientState::Waiting)];

        protocol.select(candidates.into_iter());

        let counters = protocol.counters();
        let expected = Counters {
            selected: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(protocol.next_event().unwrap(), Event::RunSelection(1));
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Selected)
        );

        let resp = protocol.rendez_vous(client_id, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            ignored: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(RendezVousResponse::Accept, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Ignored)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a rendez-vous request from a client that
    /// already sent a rendez-vous request, has been selected and then
    /// finished training.
    #[test]
    fn test_rendez_vous_done_client_re_send_rendez_vous() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.rendez_vous(client_id, ClientState::Done);

        let counters = protocol.counters();
        let expected = Counters {
            ignored: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(RendezVousResponse::Accept, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Ignored)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a rendez-vous request from a client that
    /// the protocol ignores. Usually a client is ignored when it got
    /// selected at some point, but then dropped out or did something
    /// un-expected.
    #[test]
    fn test_rendez_vous_done_inactive_client_re_send_rendez_vous() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.rendez_vous(client_id, ClientState::DoneAndInactive);

        let counters = protocol.counters();
        let expected = Counters {
            ignored: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(RendezVousResponse::Accept, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Ignored)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that has
    /// not yet been selected.
    #[test]
    fn test_heartbeat_timeout_waiting_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let _ = protocol.rendez_vous(client_id, ClientState::Unknown);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);

        protocol.heartbeat_timeout(client_id, ClientState::Waiting);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 0,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(protocol.next_event().unwrap(), Event::Accept(client_id));
        assert_eq!(protocol.next_event().unwrap(), Event::RunSelection(1));
        assert_eq!(protocol.next_event().unwrap(), Event::Remove(client_id));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that has
    /// already been selected.
    #[test]
    fn test_heartbeat_timeout_selected_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        let _ = protocol.rendez_vous(client_id, ClientState::Unknown);
        let candidates = vec![(client_id, ClientState::Waiting)];

        protocol.select(candidates.into_iter());

        let counters = protocol.counters();
        let expected = Counters {
            selected: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);

        protocol.heartbeat_timeout(client_id, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            selected: 0,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(protocol.next_event().unwrap(), Event::Accept(client_id));
        assert_eq!(protocol.next_event().unwrap(), Event::RunSelection(1));
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Selected)
        );
        assert_eq!(protocol.next_event().unwrap(), Event::Remove(client_id));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that
    /// isn't known by the protocol. In practice this should never
    /// happen, because the coordinator should have not started a
    /// timer for an unknown client. Therefore, this test expects a
    /// panic.
    #[test]
    #[should_panic]
    fn test_heartbeat_timeout_unknown_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        protocol.heartbeat_timeout(client_id, ClientState::Unknown);

        let counters = protocol.counters();
        let expected = Counters {
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that
    /// finished training and dropped out. In practice this should
    /// never happen, because after the client dropped out, its timer
    /// should have expired already, which is how we detected the
    /// drop-out in the first place. Therefore, this test expects a
    /// panic.
    #[test]
    #[should_panic]
    fn test_heartbeat_timeout_done_and_inactive_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        protocol.heartbeat_timeout(client_id, ClientState::DoneAndInactive);

        let counters = protocol.counters();
        let expected = Counters {
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that
    /// finished training.
    #[test]
    fn test_heartbeat_timeout_done_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.counters = Counters {
            done: 1,
            ..Default::default()
        };

        protocol.heartbeat_timeout(client_id, ClientState::Done);

        let counters = protocol.counters();
        let expected = Counters {
            done: 1, // <- Not sure about this. Shouldn't it be 0?
            done_and_inactive: 1,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(protocol.next_event().unwrap(), Event::Remove(client_id));
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::DoneAndInactive)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat timeout for a client that the
    /// protocol ignores.
    #[test]
    fn test_heartbeat_timeout_ignore_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.counters = Counters {
            ignored: 1,
            ..Default::default()
        };

        protocol.heartbeat_timeout(client_id, ClientState::Ignored);

        let counters = protocol.counters();
        let expected = Counters {
            ignored: 0,
            ..Default::default()
        };

        assert_eq!(counters, expected);
        assert_eq!(protocol.next_event().unwrap(), Event::Remove(client_id));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client that the
    /// protocol doesn't know about.
    #[test]
    fn test_heartbeat_unknown_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::Unknown);

        assert_eq!(HeartBeatResponse::Reject, resp);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client that finished
    /// training and dropped out already.
    #[test]
    fn test_heartbeat_done_and_inactive_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::DoneAndInactive);

        assert_eq!(HeartBeatResponse::Reject, resp);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client that the
    /// protocol ignores. Usually a client is ignored when it got
    /// selected at some point, but then dropped out or did something
    /// un-expected.
    #[test]
    fn test_heartbeat_ignore_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::Ignored);

        assert_eq!(HeartBeatResponse::StandBy, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::ResetHeartBeat(client_id)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client has not been
    /// selected yet.
    #[test]
    fn test_heartbeat_waiting_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::Waiting);

        assert_eq!(HeartBeatResponse::StandBy, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::ResetHeartBeat(client_id)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client that finished
    /// training and is still active (ie didn't drop out).
    #[test]
    fn test_heartbeat_done_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::Done);

        assert_eq!(HeartBeatResponse::StandBy, resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::ResetHeartBeat(client_id)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client that has been
    /// selected but hasn't finished training yet.
    #[test]
    fn test_heartbeat_selected_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();

        let resp = protocol.heartbeat(client_id, ClientState::Selected);

        assert_eq!(HeartBeatResponse::Round(0), resp);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::ResetHeartBeat(client_id)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a heartbeat from a client in any state
    /// after all the rounds have been completed already.
    #[test]
    fn test_heartbeat_training_complete() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.is_training_complete = true;
        let client_states = vec![
            ClientState::Unknown,
            ClientState::Ignored,
            ClientState::Done,
            ClientState::DoneAndInactive,
            ClientState::Selected,
            ClientState::Waiting,
        ];

        for state in client_states.iter() {
            let resp = protocol.heartbeat(client_id, *state);

            assert_eq!(HeartBeatResponse::Finish, resp);
            assert_eq!(
                protocol.next_event().unwrap(),
                Event::ResetHeartBeat(client_id)
            );
        }
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a start training request from a client
    /// that has been selected and has not finished training.
    #[test]
    fn test_start_training_selected_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());

        let resp = protocol.start_training(ClientState::Selected);

        assert_eq!(StartTrainingResponse::Accept, resp);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a start training request from a client
    /// that has been selected and has already finished training.
    #[test]
    fn test_start_training_selected_participant_training_complete() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.is_training_complete = true;

        let resp = protocol.start_training(ClientState::Selected);

        assert_eq!(StartTrainingResponse::Reject, resp);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a start training request from a client
    /// that has not been selected.
    #[test]
    fn test_start_training_with_not_selected_participant() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_states = vec![
            ClientState::Unknown,
            ClientState::Ignored,
            ClientState::Done,
            ClientState::DoneAndInactive,
            ClientState::Waiting,
        ];

        for state in client_states.iter() {
            let resp = protocol.start_training(*state);

            assert_eq!(StartTrainingResponse::Reject, resp);
        }
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a valid end training request when all the
    /// rounds have already been completed. An end training request is
    /// valid when it is for a participant that has been selected and
    /// has not finished training yet.
    #[test]
    fn test_end_training_is_training_complete() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.is_training_complete = true;

        protocol.end_training(client_id, true, ClientState::Selected);
        // FIXME: add checks
    }

    /// Test the outcome of a valid end training request while the
    /// protocol is waiting for an ongoing aggregation to finish. An
    /// end training request is valid when it is for a participant
    /// that has been selected and has not finished training yet.
    #[test]
    fn test_end_training_waiting_for_aggregation() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.waiting_for_aggregation = true;

        protocol.end_training(client_id, true, ClientState::Selected);

        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a valid end training request when the
    /// protocol is still waiting for several clients to finish
    /// training (ie this end training request isn't the one that
    /// completes the current round). An end training request is valid
    /// when it is for a participant that has been selected and has
    /// not finished training yet.
    #[test]
    fn test_end_training_selected_participant_success_not_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.counters = Counters {
            waiting: 0,
            selected: 2,
            done: 5,
            done_and_inactive: 3,
            ignored: 2,
        };

        protocol.end_training(client_id, true, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 0,
            selected: 1,
            done: 6,
            done_and_inactive: 3,
            ignored: 2,
        };

        assert_eq!(counters, expected);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Done)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a valid end training request that
    /// completes the current round. An end training request is valid
    /// when it is for a participant that has been selected and has
    /// not finished training yet.
    #[test]
    fn test_end_training_selected_participant_success_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        // rounds start at 0. The settings specify two rounds, so the
        // last round correspond to current_round = 1
        protocol.current_round = 1;
        let client_id = ClientId::new();
        protocol.counters = Counters {
            waiting: 0,
            selected: 1,
            done: 5,
            done_and_inactive: 3,
            ignored: 2,
        };

        protocol.end_training(client_id, true, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 1 + 5 + 2,
            selected: 0,
            done: 0,
            done_and_inactive: 0,
            ignored: 0,
        };

        assert_eq!(counters, expected);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Done)
        );
        assert_eq!(protocol.next_event().unwrap(), Event::RunAggregation);
        assert_eq!(protocol.next_event().unwrap(), Event::ResetAll);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a valid end training request that has been
    /// rejected by the aggregator. It is still valid in the sense
    /// that it corresponds to a client for which the protocol expects
    /// an end training request.
    #[test]
    fn test_end_training_selected_participant_no_success() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        let client_id = ClientId::new();
        protocol.counters = Counters {
            waiting: 0,
            selected: 2,
            done: 5,
            done_and_inactive: 3,
            ignored: 2,
        };

        protocol.end_training(client_id, false, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 0,
            selected: 1,
            done: 5,
            done_and_inactive: 3,
            ignored: 3,
        };

        assert_eq!(counters, expected);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Ignored)
        );
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of a valid end training request that has been
    /// rejected by the aggregator, and that should trigger a new
    /// selection.
    #[test]
    fn test_end_training_selected_participant_no_success_run_selection() {
        let fl_settings = FederatedLearningSettings {
            rounds: 1,
            participants_ratio: 1.0,
            min_clients: 15,
            heartbeat_timeout: 15,
        };
        let mut protocol = Protocol::new(fl_settings);
        let client_id = ClientId::new();
        protocol.counters = Counters {
            waiting: 6,
            selected: 2,
            done: 5,
            done_and_inactive: 3,
            ignored: 2,
        };

        protocol.end_training(client_id, false, ClientState::Selected);

        let counters = protocol.counters();
        let expected = Counters {
            waiting: 6,
            selected: 1,
            done: 5,
            done_and_inactive: 3,
            ignored: 3,
        };
        assert_eq!(counters, expected);
        assert_eq!(
            protocol.next_event().unwrap(),
            Event::SetState(client_id, ClientState::Ignored)
        );
        assert_eq!(protocol.next_event().unwrap(), Event::RunSelection(6));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of calling `end_aggregation` while there's
    /// not ongoing aggregation.
    #[test]
    fn test_end_aggregation_not_waiting_for_aggregation() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.end_aggregation(false);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of an aggregation completion.
    #[test]
    fn test_end_aggregation_waiting_for_aggregation_success_not_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.counters = Counters {
            selected: 1,
            ..Default::default()
        };
        protocol.waiting_for_aggregation = true;
        protocol.end_aggregation(true);

        assert_eq!(protocol.waiting_for_aggregation, false);
        assert_eq!(protocol.next_event().unwrap(), Event::EndRound(0));
        assert_eq!(protocol.current_round, 1);
        assert_eq!(protocol.is_training_complete, false);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of an aggregation completion in the last round.
    #[test]
    fn test_end_aggregation_waiting_for_aggregation_success_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.counters = Counters {
            selected: 1,
            ..Default::default()
        };
        // rounds start at 0. The settings specify two rounds, so the
        // last round correspond to current_round = 1
        protocol.current_round = 1;
        protocol.waiting_for_aggregation = true;
        protocol.end_aggregation(true);

        assert_eq!(protocol.waiting_for_aggregation, false);
        assert_eq!(protocol.current_round, 2);
        assert_eq!(protocol.is_training_complete, true);
        assert_eq!(protocol.next_event().unwrap(), Event::EndRound(1));
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of an aggregation failure.
    #[test]
    fn test_end_aggregation_waiting_for_aggregation_no_success_not_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.counters = Counters {
            selected: 1,
            ..Default::default()
        };
        protocol.waiting_for_aggregation = true;
        protocol.end_aggregation(false);

        assert_eq!(protocol.waiting_for_aggregation, false);
        assert_eq!(protocol.is_training_complete, false);
        assert!(protocol.next_event().is_none());
    }

    /// Test the outcome of an aggregation failure in the last round.
    #[test]
    fn test_end_aggregation_waiting_for_aggregation_no_success_last_round() {
        let mut protocol = Protocol::new(get_default_fl_settings());
        protocol.counters = Counters {
            selected: 1,
            ..Default::default()
        };
        // rounds start at 0. The settings specify two rounds, so the
        // last round correspond to current_round = 1
        protocol.current_round = 1;
        protocol.waiting_for_aggregation = true;
        protocol.end_aggregation(false);

        assert_eq!(protocol.waiting_for_aggregation, false);
        assert_eq!(protocol.is_training_complete, false);
        assert_eq!(protocol.current_round, 1);
        assert!(protocol.next_event().is_none());
    }

    fn create_participant(protocol: &mut Protocol) -> ClientId {
        let new_client = ClientId::new();
        protocol.rendez_vous(new_client, ClientState::Unknown);
        new_client
    }

    fn select_and_start_training(
        protocol: &mut Protocol,
        candidates: Vec<(ClientId, ClientState)>,
    ) {
        let number_of_candidates = candidates.len();

        protocol.select(candidates.into_iter());

        for _ in 0..number_of_candidates {
            protocol.start_training(ClientState::Selected);
        }
    }

    fn end_training(protocol: &mut Protocol, candidates: Vec<(ClientId, ClientState)>) {
        for (client_id, _) in candidates.into_iter() {
            protocol.end_training(client_id, true, ClientState::Selected);
        }
    }

    /// Simple test case with two particpants and two rounds.  After
    /// the last round the coordinator should response with a
    /// StartTrainingResponse::Reject for each new start_training
    /// request.
    #[test]
    fn test_case_1() {
        let n_of_rounds = 2;
        let n_of_clients = 2;

        let settings = FederatedLearningSettings {
            rounds: n_of_rounds,
            participants_ratio: 1.0,
            min_clients: n_of_clients,
            heartbeat_timeout: 15,
        };

        let mut protocol = Protocol::new(settings);
        let mut clients = Vec::new();
        for _ in 0..n_of_clients {
            clients.push((create_participant(&mut protocol), ClientState::Waiting))
        }

        for round in 0..n_of_rounds {
            select_and_start_training(&mut protocol, clients.clone());
            let counters = protocol.counters();
            let expected = Counters {
                selected: 2,
                ..Default::default()
            };
            assert_eq!(counters, expected);

            end_training(&mut protocol, clients.clone());
            let counters = protocol.counters();
            let expected = Counters {
                waiting: 2,
                ..Default::default()
            };
            assert_eq!(counters, expected);
            assert_eq!(protocol.current_round, round);

            protocol.end_aggregation(true);
            assert_eq!(protocol.current_round, round + 1);
        }

        let try_start_after_last_round = protocol.start_training(ClientState::Selected);
        assert_eq!(try_start_after_last_round, StartTrainingResponse::Reject);
    }
}
