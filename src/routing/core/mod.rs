// Copyright 2021 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod connectivity;
mod messaging;
mod public_api;

use super::{
    command::Command, connectivity_complaints::ConnectivityComplaints,
    enduser_registry::EndUserRegistry, lazy_messaging, split_barrier::SplitBarrier,
};
use crate::{
    agreement::{DkgVoter, Proposal, ProposalAggregator},
    error::Result,
    event::{Elders, Event, NodeElderChange},
    message_filter::MessageFilter,
    messages::Message,
    network::Network,
    node::Node,
    relocation::RelocateState,
    section::{Section, SectionKeyShare, SectionKeysProvider},
};
use bls_signature_aggregator::SignatureAggregator;
use itertools::Itertools;
use resource_proof::ResourceProof;
use std::collections::BTreeSet;
use tokio::sync::mpsc;
use xor_name::{Prefix, XorName};

pub(crate) const RESOURCE_PROOF_DATA_SIZE: usize = 64;
pub(crate) const RESOURCE_PROOF_DIFFICULTY: u8 = 2;
const KEY_CACHE_SIZE: u8 = 5;

// State + logic of a routing node.
pub(crate) struct Core {
    node: Node,
    section: Section,
    network: Network,
    section_keys_provider: SectionKeysProvider,
    message_aggregator: SignatureAggregator,
    proposal_aggregator: ProposalAggregator,
    split_barrier: SplitBarrier,
    // Voter for Dkg
    dkg_voter: DkgVoter,
    relocate_state: Option<RelocateState>,
    msg_filter: MessageFilter,
    pub(super) event_tx: mpsc::UnboundedSender<Event>,
    joins_allowed: bool,
    resource_proof: ResourceProof,
    end_users: EndUserRegistry,
    connectivity_complaints: ConnectivityComplaints,
}

impl Core {
    // Creates `Core` for a regular node.
    pub fn new(
        node: Node,
        section: Section,
        section_key_share: Option<SectionKeyShare>,
        event_tx: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let section_keys_provider = SectionKeysProvider::new(KEY_CACHE_SIZE, section_key_share);

        Self {
            node,
            section,
            network: Network::new(),
            section_keys_provider,
            proposal_aggregator: ProposalAggregator::default(),
            split_barrier: SplitBarrier::new(),
            message_aggregator: SignatureAggregator::default(),
            dkg_voter: DkgVoter::default(),
            relocate_state: None,
            msg_filter: MessageFilter::new(),
            event_tx,
            joins_allowed: true,
            resource_proof: ResourceProof::new(RESOURCE_PROOF_DATA_SIZE, RESOURCE_PROOF_DIFFICULTY),
            end_users: EndUserRegistry::new(),
            connectivity_complaints: ConnectivityComplaints::new(),
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    // Miscellaneous
    ////////////////////////////////////////////////////////////////////////////

    // Update our knowledge of their (sender's) section and their knowledge of our section.
    pub(crate) fn update_section_knowledge(&mut self, msg: &Message) -> Result<Vec<Command>> {
        if !self.is_elder() {
            return Ok(vec![]);
        }

        let actions = lazy_messaging::process(&self.node, &self.section, &self.network, msg)?;
        let mut commands = vec![];

        if let Some(msg) = actions.send {
            commands.extend(self.relay_message(&msg)?);
        }

        if let Some(proposal) = actions.propose {
            commands.extend(self.propose(proposal)?);
        }

        Ok(commands)
    }

    pub(crate) fn state_snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            is_elder: self.is_elder(),
            last_key: *self.section.chain().last_key(),
            prefix: *self.section.prefix(),
            elders: self
                .section()
                .authority_provider()
                .elders()
                .keys()
                .copied()
                .collect(),
        }
    }

    pub(crate) fn update_state(&mut self, old: StateSnapshot) -> Result<Vec<Command>> {
        let mut commands = vec![];
        let new = self.state_snapshot();

        self.section_keys_provider
            .finalise_dkg(self.section.chain().last_key());

        if new.prefix != old.prefix {
            info!("Split");
        }

        if new.last_key != old.last_key {
            self.msg_filter.reset();

            if new.is_elder {
                info!(
                    "Section updated: prefix: ({:b}), key: {:?}, elders: {}",
                    new.prefix,
                    new.last_key,
                    self.section.authority_provider().peers().format(", ")
                );

                if self.section_keys_provider.has_key_share() {
                    commands.extend(self.promote_and_demote_elders()?);
                    // Whenever there is an elders change, casting a round of joins_allowed
                    // proposals to sync.
                    commands.extend(self.propose(Proposal::JoinsAllowed(self.joins_allowed))?);
                }

                self.print_network_stats();
            }

            if new.is_elder || old.is_elder {
                commands.extend(self.send_sync(self.section.clone(), self.network.clone())?);
            }

            let current: BTreeSet<_> = self
                .section
                .authority_provider()
                .elders()
                .keys()
                .copied()
                .collect();
            let added = current.difference(&old.elders).copied().collect();
            let removed = old.elders.difference(&current).copied().collect();
            let remaining = old.elders.intersection(&current).copied().collect();

            let elders = Elders {
                prefix: new.prefix,
                key: new.last_key,
                remaining,
                added,
                removed,
            };

            let self_status_change = if !old.is_elder && new.is_elder {
                info!("Promoted to elder");
                NodeElderChange::Promoted
            } else if old.is_elder && !new.is_elder {
                info!("Demoted");
                self.network = Network::new();
                self.section_keys_provider = SectionKeysProvider::new(KEY_CACHE_SIZE, None);
                NodeElderChange::Demoted
            } else {
                NodeElderChange::None
            };

            let sibling_elders = if new.prefix != old.prefix {
                self.network.get(&new.prefix.sibling()).map(|sec_auth| {
                    let current: BTreeSet<_> = sec_auth.elders().keys().copied().collect();
                    let added = current.difference(&old.elders).copied().collect();
                    let removed = old.elders.difference(&current).copied().collect();
                    let remaining = old.elders.intersection(&current).copied().collect();
                    Elders {
                        prefix: new.prefix.sibling(),
                        key: sec_auth.section_key(),
                        remaining,
                        added,
                        removed,
                    }
                })
            } else {
                None
            };

            let event = if let Some(sibling_elders) = sibling_elders {
                Event::SectionSplit {
                    elders,
                    sibling_elders,
                    self_status_change,
                }
            } else {
                Event::EldersChanged {
                    elders,
                    self_status_change,
                }
            };

            self.send_event(event);
        }

        if !new.is_elder {
            commands.extend(self.return_relocate_promise());
        }

        Ok(commands)
    }

    pub(crate) fn section_key_by_name(&self, name: &XorName) -> &bls::PublicKey {
        if self.section.prefix().matches(name) {
            self.section.chain().last_key()
        } else if let Some(key) = self.network.key_by_name(name) {
            key
        } else if self.section.prefix().sibling().matches(name) {
            // For sibling with unknown key, use the previous key in our chain under the assumption
            // that it's the last key before the split and therefore the last key of theirs we know.
            // In case this assumption is not correct (because we already progressed more than one
            // key since the split) then this key would be unknown to them and they would send
            // us back their whole section chain. However, this situation should be rare.
            self.section.chain().prev_key()
        } else {
            self.section.chain().root_key()
        }
    }

    pub(crate) fn print_network_stats(&self) {
        self.network
            .network_stats(self.section.authority_provider())
            .print()
    }
}

pub(crate) struct StateSnapshot {
    is_elder: bool,
    last_key: bls::PublicKey,
    prefix: Prefix,
    elders: BTreeSet<XorName>,
}
