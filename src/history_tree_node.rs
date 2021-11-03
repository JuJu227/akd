// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

use crate::serialization::{from_digest, to_digest};
use crate::storage::types::{DbRecord, StorageType};
use crate::storage::{Storable, V2Storage};
use crate::{node_state::*, Direction, ARITY};
use async_recursion::async_recursion;
use winter_crypto::Hasher;

use crate::errors::{HistoryTreeNodeError, StorageError};

use std::marker::{PhantomData, Send, Sync};

use serde::{Deserialize, Serialize};

#[derive(PartialEq, Debug, Copy, Clone, Serialize, Deserialize)]
pub enum NodeType {
    Leaf = 1,
    Root = 2,
    Interior = 3,
}

impl NodeType {
    pub(crate) fn from_u8(code: u8) -> Self {
        match code {
            1 => Self::Leaf,
            2 => Self::Root,
            3 => Self::Interior,
            _ => Self::Leaf,
        }
    }
}

pub type HistoryInsertionNode<H> = (Direction, HistoryChildState<H>);
pub type HistoryNodeHash<H> = Option<H>;

/**
 * HistoryNode will represent a generic interior node of a compressed history tree
 **/
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct HistoryTreeNode<H> {
    pub label: NodeLabel,
    pub location: usize,
    pub epochs: Vec<u64>,
    pub parent: usize,
    // Just use usize and have the 0th position be empty and that can be the parent of root. This makes things simpler.
    pub node_type: NodeType,
    // Note that the NodeType along with the parent/children being options
    // allows us to use this struct to represent child and parent nodes as well.
    pub _h: PhantomData<H>,
}

// parameters are azks_id and location
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NodeKey(pub usize);

impl<H: Hasher> Storable for HistoryTreeNode<H> {
    type Key = NodeKey;

    fn data_type() -> StorageType {
        StorageType::HistoryTreeNode
    }

    fn get_id(&self) -> NodeKey {
        NodeKey(self.location)
    }
}

unsafe impl<H: Hasher> Sync for HistoryTreeNode<H> {}

impl<H: Hasher> Clone for HistoryTreeNode<H> {
    fn clone(&self) -> Self {
        Self {
            label: self.label,
            location: self.location,
            epochs: self.epochs.clone(),
            parent: self.parent,
            node_type: self.node_type,
            _h: PhantomData,
        }
    }
}

impl<H: Hasher + Send + Sync> HistoryTreeNode<H> {
    fn new(label: NodeLabel, location: usize, parent: usize, node_type: NodeType) -> Self {
        HistoryTreeNode {
            label,
            location,
            epochs: vec![],
            parent, // Root node is its own parent
            node_type,
            _h: PhantomData,
        }
    }

    pub(crate) async fn write_to_storage<S: V2Storage + Send + Sync>(
        &self,
        storage: &S,
    ) -> Result<(), StorageError> {
        storage
            .set::<H>(DbRecord::HistoryTreeNode(self.clone()))
            .await
    }

    pub(crate) async fn get_from_storage<S: V2Storage + Send + Sync>(
        storage: &S,
        key: NodeKey,
    ) -> Result<HistoryTreeNode<H>, StorageError> {
        let record = storage.get::<H, HistoryTreeNode<H>>(key).await?;
        match record {
            DbRecord::HistoryTreeNode(node) => Ok(node),
            _ => Err(StorageError::GetError(String::from("Not found"))),
        }
    }

    // Inserts a single leaf node and updates the required hashes
    pub(crate) async fn insert_single_leaf<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        new_leaf: Self,
        epoch: u64,
        num_nodes: &mut usize,
    ) -> Result<(), HistoryTreeNodeError> {
        self.insert_single_leaf_helper(storage, new_leaf, epoch, num_nodes, true)
            .await
    }

    // Inserts a single leaf node
    pub(crate) async fn insert_single_leaf_without_hash<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        new_leaf: Self,
        epoch: u64,
        num_nodes: &mut usize,
    ) -> Result<(), HistoryTreeNodeError> {
        self.insert_single_leaf_helper(storage, new_leaf, epoch, num_nodes, false)
            .await
    }

    // Inserts a single leaf node and updates the required hashes,
    // if hashing is true
    #[async_recursion]
    pub(crate) async fn insert_single_leaf_helper<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        mut new_leaf: Self,
        epoch: u64,
        num_nodes: &mut usize,
        hashing: bool,
    ) -> Result<(), HistoryTreeNodeError> {
        let (lcs_label, dir_leaf, dir_self) = self
            .label
            .get_longest_common_prefix_and_dirs(new_leaf.get_label());

        if self.is_root() {
            new_leaf.location = *num_nodes;
            new_leaf.write_to_storage(storage).await?;
            *num_nodes += 1;
            // the root should always be instantiated with dummy children in the beginning
            let child_state = self
                .get_child_at_epoch(storage, self.get_latest_epoch()?, dir_leaf)
                .await?;
            if child_state.dummy_marker == DummyChildState::Dummy {
                new_leaf.parent = self.location;
                self.set_node_child_without_hash(storage, epoch, dir_leaf, &new_leaf)
                    .await?;
                self.write_to_storage(storage).await?;
                new_leaf.write_to_storage(storage).await?;

                if hashing {
                    new_leaf.update_hash(storage, epoch).await?;
                    let mut new_self: HistoryTreeNode<H> =
                        HistoryTreeNode::get_from_storage(storage, NodeKey(self.location)).await?;
                    new_self.update_hash(storage, epoch).await?;
                }

                *self = HistoryTreeNode::get_from_storage(storage, NodeKey(self.location)).await?;
                return Ok(());
            }
        }
        // if a node is the longest common prefix of itself and the leaf, dir_self will be None
        match dir_self {
            Some(_) => {
                // This is the case where the calling node and the leaf have a longest common prefix
                // not equal to the label of the calling node.
                // This means that the current node needs to be pushed down one level (away from root)
                // in the tree and replaced with a new node whose label is equal to the longest common prefix.
                let mut parent =
                    HistoryTreeNode::get_from_storage(storage, NodeKey(self.parent)).await?;
                let self_dir_in_parent = parent.get_direction_at_ep(storage, self, epoch).await?;
                let new_node_location = *num_nodes;

                let mut new_node = HistoryTreeNode::new(
                    lcs_label,
                    new_node_location,
                    parent.location,
                    NodeType::Interior,
                );
                new_node.epochs.push(epoch);
                new_node.write_to_storage(storage).await?;
                *num_nodes += 1;
                // Add this node in the correct dir and child node in the other direction
                new_leaf.parent = new_node.location;
                new_leaf.write_to_storage(storage).await?;

                self.parent = new_node.location;
                self.write_to_storage(storage).await?;

                new_node
                    .set_node_child_without_hash(storage, epoch, dir_leaf, &new_leaf)
                    .await?;
                new_node
                    .set_node_child_without_hash(storage, epoch, dir_self, self)
                    .await?;

                parent
                    .set_node_child_without_hash(storage, epoch, self_dir_in_parent, &new_node)
                    .await?;
                if hashing {
                    new_leaf.update_hash(storage, epoch).await?;
                    self.update_hash(storage, epoch).await?;
                    new_node =
                        HistoryTreeNode::get_from_storage(storage, NodeKey(new_node.location))
                            .await?;
                    new_node.update_hash(storage, epoch).await?;
                }
                new_node.write_to_storage(storage).await?;
                parent.write_to_storage(storage).await?;
                *self = HistoryTreeNode::get_from_storage(storage, NodeKey(self.location)).await?;
                Ok(())
            }
            None => {
                // case where the current node is equal to the lcs
                let child_st = self
                    .get_child_at_epoch(storage, self.get_latest_epoch()?, dir_leaf)
                    .await?;

                match child_st.dummy_marker {
                    DummyChildState::Dummy => {
                        Err(HistoryTreeNodeError::CompressionError(self.label))
                    }
                    DummyChildState::Real => {
                        let mut child_node =
                            HistoryTreeNode::get_from_storage(storage, NodeKey(child_st.location))
                                .await?;
                        child_node
                            .insert_single_leaf_helper(storage, new_leaf, epoch, num_nodes, hashing)
                            .await?;
                        if hashing {
                            *self =
                                HistoryTreeNode::get_from_storage(storage, NodeKey(self.location))
                                    .await?;
                            self.update_hash(storage, epoch).await?;
                            self.write_to_storage(storage).await?;
                        }
                        *self = HistoryTreeNode::get_from_storage(storage, NodeKey(self.location))
                            .await?;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Updates the hash of this node as stored in its parent,
    /// provided the children of this node have already updated their own versions
    /// in this node and epoch is contained in the state_map
    /// Also assumes that `set_child_without_hash` has already been called
    pub(crate) async fn update_hash<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        epoch: u64,
    ) -> Result<(), HistoryTreeNodeError> {
        match self.node_type {
            NodeType::Leaf => {
                // the hash of this is just the value, simply place in parent
                let leaf_hash_val =
                    H::merge(&[self.get_value(storage).await?, hash_label::<H>(self.label)]);
                self.update_hash_at_parent(storage, epoch, leaf_hash_val)
                    .await
            }
            _ => {
                // the root has no parent, so the hash must only be stored within the value
                let mut hash_digest = self.hash_node(storage, epoch).await?;
                if self.is_root() {
                    hash_digest = H::merge(&[hash_digest, hash_label::<H>(self.label)]);
                }
                let epoch_state = self.get_state_at_epoch(storage, epoch).await?;

                let mut updated_state = epoch_state;
                updated_state.value = from_digest::<H>(hash_digest)?;
                updated_state.key = NodeStateKey(self.label, epoch);
                set_state_map(storage, updated_state).await?;

                self.write_to_storage(storage).await?;
                let hash_digest = H::merge(&[hash_digest, hash_label::<H>(self.label)]);
                self.update_hash_at_parent(storage, epoch, hash_digest)
                    .await
            }
        }
    }

    async fn hash_node<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
    ) -> Result<H::Digest, HistoryTreeNodeError> {
        let epoch_node_state = self.get_state_at_epoch(storage, epoch).await?;
        let mut new_hash = H::hash(&[]);
        for child_index in 0..ARITY {
            new_hash = H::merge(&[
                new_hash,
                to_digest::<H>(
                    &epoch_node_state
                        .get_child_state_in_dir(child_index)
                        .hash_val,
                )
                .unwrap(),
            ]);
        }
        Ok(new_hash)
    }

    async fn update_hash_at_parent<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        epoch: u64,
        new_hash_val: H::Digest,
    ) -> Result<(), HistoryTreeNodeError> {
        if self.is_root() {
            Ok(())
        } else {
            let parent =
                &mut HistoryTreeNode::get_from_storage(storage, NodeKey(self.parent)).await?;
            if parent.get_latest_epoch()? < epoch {
                let (_, dir_self, _) = parent
                    .label
                    .get_longest_common_prefix_and_dirs(self.get_label());
                parent
                    .set_node_child_without_hash(storage, epoch, dir_self, self)
                    .await?;
                parent.write_to_storage(storage).await?;
                *parent = HistoryTreeNode::get_from_storage(storage, NodeKey(self.parent)).await?;
            }
            match get_state_map(storage, parent, epoch).await {
                Err(_) => Err(HistoryTreeNodeError::ParentNextEpochInvalid(epoch)),
                Ok(parent_state) => match parent.get_direction_at_ep(storage, self, epoch).await? {
                    None => Err(HistoryTreeNodeError::HashUpdateOnlyAllowedAfterNodeInsertion),
                    Some(s_dir) => {
                        let mut parent_updated_state = parent_state;
                        let mut self_child_state =
                            parent_updated_state.get_child_state_in_dir(s_dir);
                        self_child_state.hash_val = from_digest::<H>(new_hash_val)?;
                        parent_updated_state.child_states[s_dir] = self_child_state;
                        parent_updated_state.key = NodeStateKey(parent.label, epoch);
                        set_state_map(storage, parent_updated_state).await?;
                        parent.write_to_storage(storage).await?;

                        Ok(())
                    }
                },
            }
        }
    }

    #[async_recursion]
    pub(crate) async fn set_child_without_hash<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        epoch: u64,
        child: &HistoryInsertionNode<H>,
    ) -> Result<(), HistoryTreeNodeError> {
        let (direction, child_node) = child.clone();
        match direction {
            Direction::Some(dir) => match get_state_map(storage, self, epoch).await {
                Ok(HistoryNodeState {
                    value,
                    mut child_states,
                    key: _,
                }) => {
                    child_states[dir] = child_node;
                    set_state_map(
                        storage,
                        HistoryNodeState {
                            value,
                            child_states,
                            key: NodeStateKey(self.label, epoch),
                        },
                    )
                    .await?;
                    Ok(())
                }
                Err(_) => {
                    set_state_map(
                        storage,
                        match self
                            .get_state_at_epoch(storage, self.get_latest_epoch()?)
                            .await
                        {
                            Ok(mut latest_st) => {
                                latest_st.key = NodeStateKey(self.label, epoch);
                                latest_st
                            }
                            Err(_) => HistoryNodeState::<H>::new(NodeStateKey(self.label, epoch)),
                        },
                    )
                    .await?;

                    match self.get_latest_epoch() {
                        Ok(latest) => {
                            if latest != epoch {
                                self.epochs.push(epoch);
                            }
                        }
                        Err(_) => {
                            self.epochs.push(epoch);
                        }
                    }
                    self.write_to_storage(storage).await?;
                    self.set_child_without_hash(storage, epoch, child).await
                }
            },
            Direction::None => Err(HistoryTreeNodeError::NoDirectionInSettingChild(
                self.get_label().get_val(),
                child_node.label.get_val(),
            )),
        }
    }

    pub(crate) async fn set_node_child_without_hash<S: V2Storage + Sync + Send>(
        &mut self,
        storage: &S,
        epoch: u64,
        dir: Direction,
        child: &Self,
    ) -> Result<(), HistoryTreeNodeError> {
        let node_as_child_state = child.to_node_unhashed_child_state(storage).await?;
        let insertion_node = (dir, node_as_child_state);
        self.set_child_without_hash(storage, epoch, &insertion_node)
            .await
    }

    ////// getrs for this node ////

    pub(crate) async fn get_value_at_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
    ) -> Result<H::Digest, HistoryTreeNodeError> {
        Ok(to_digest::<H>(&self.get_state_at_epoch(storage, epoch).await?.value).unwrap())
    }

    pub(crate) async fn get_value_without_label_at_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
    ) -> Result<H::Digest, HistoryTreeNodeError> {
        if self.is_leaf() {
            return self.get_value_at_epoch(storage, epoch).await;
        }
        let children = self.get_state_at_epoch(storage, epoch).await?.child_states;
        let mut new_hash = H::hash(&[]);
        for child in children.iter().take(ARITY) {
            new_hash = H::merge(&[new_hash, to_digest::<H>(&child.hash_val).unwrap()]);
        }
        Ok(new_hash)
    }

    pub(crate) async fn get_child_location_at_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
        dir: Direction,
    ) -> Result<usize, HistoryTreeNodeError> {
        Ok(self.get_child_at_epoch(storage, epoch, dir).await?.location)
    }

    // gets value at current epoch
    pub(crate) async fn get_value<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
    ) -> Result<H::Digest, HistoryTreeNodeError> {
        Ok(get_state_map(storage, self, self.get_latest_epoch()?)
            .await
            .map(|node_state| to_digest::<H>(&node_state.value).unwrap())?)
    }

    pub(crate) fn get_birth_epoch(&self) -> u64 {
        self.epochs[0]
    }

    fn get_label(&self) -> NodeLabel {
        self.label
    }

    // gets the direction of node, i.e. if it's a left
    // child or right. If not found, return None
    async fn get_direction_at_ep<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        node: &Self,
        ep: u64,
    ) -> Result<Direction, HistoryTreeNodeError> {
        let mut outcome: Direction = None;
        let state_at_ep = self.get_state_at_epoch(storage, ep).await?;
        for node_index in 0..ARITY {
            let node_val = state_at_ep.get_child_state_in_dir(node_index);
            let node_label = node_val.label;
            if node_label == node.get_label() {
                outcome = Some(node_index)
            }
        }
        Ok(outcome)
    }

    pub(crate) fn is_root(&self) -> bool {
        matches!(self.node_type, NodeType::Root)
    }

    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self.node_type, NodeType::Leaf)
    }

    ///// getrs for child nodes ////

    pub(crate) async fn get_child_at_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
        direction: Direction,
    ) -> Result<HistoryChildState<H>, HistoryTreeNodeError> {
        match direction {
            Direction::None => Err(HistoryTreeNodeError::DirectionIsNone),
            Direction::Some(dir) => {
                if self.get_birth_epoch() > epoch {
                    Err(HistoryTreeNodeError::NoChildInTreeAtEpoch(epoch, dir))
                } else {
                    let mut chosen_ep = self.get_birth_epoch();
                    for existing_ep in &self.epochs {
                        if *existing_ep <= epoch {
                            chosen_ep = *existing_ep;
                        }
                    }
                    self.get_child_at_existing_epoch(storage, chosen_ep, direction)
                        .await
                }
            }
        }
    }

    pub(crate) async fn get_child_at_existing_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
        direction: Direction,
    ) -> Result<HistoryChildState<H>, HistoryTreeNodeError> {
        match direction {
            Direction::None => Err(HistoryTreeNodeError::DirectionIsNone),
            Direction::Some(dir) => Ok(get_state_map(storage, self, epoch)
                .await
                .map(|curr| curr.get_child_state_in_dir(dir))?),
        }
    }

    pub(crate) async fn get_state_at_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
    ) -> Result<HistoryNodeState<H>, HistoryTreeNodeError> {
        if self.get_birth_epoch() > epoch {
            Err(HistoryTreeNodeError::NodeDidNotExistAtEp(self.label, epoch))
        } else {
            let mut chosen_ep = self.get_birth_epoch();
            for existing_ep in &self.epochs {
                if *existing_ep <= epoch {
                    chosen_ep = *existing_ep;
                }
            }
            self.get_state_at_existing_epoch(storage, chosen_ep).await
        }
    }

    async fn get_state_at_existing_epoch<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
        epoch: u64,
    ) -> Result<HistoryNodeState<H>, HistoryTreeNodeError> {
        get_state_map(storage, self, epoch)
            .await
            .map_err(|_| HistoryTreeNodeError::NodeDidNotHaveExistingStateAtEp(self.label, epoch))
    }

    /* Functions for compression-related operations */

    pub(crate) fn get_latest_epoch(&self) -> Result<u64, HistoryTreeNodeError> {
        match self.epochs.len() {
            0 => Err(HistoryTreeNodeError::NodeCreatedWithoutEpochs(
                self.label.get_val(),
            )),
            n => Ok(self.epochs[n - 1]),
        }
    }

    /////// Helpers /////////

    async fn to_node_unhashed_child_state<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
    ) -> Result<HistoryChildState<H>, HistoryTreeNodeError> {
        Ok(HistoryChildState {
            dummy_marker: DummyChildState::Real,
            location: self.location,
            label: self.label,
            hash_val: from_digest::<H>(H::merge(&[
                self.get_value(storage).await?,
                hash_label::<H>(self.label),
            ]))?,
            epoch_version: self.get_latest_epoch()?,
            _h: PhantomData,
        })
    }

    #[cfg(test)]
    pub async fn to_node_child_state<S: V2Storage + Sync + Send>(
        &self,
        storage: &S,
    ) -> Result<HistoryChildState<H>, HistoryTreeNodeError> {
        Ok(HistoryChildState {
            dummy_marker: DummyChildState::Real,
            location: self.location,
            label: self.label,
            hash_val: from_digest::<H>(H::merge(&[
                self.get_value(storage).await?,
                hash_label::<H>(self.label),
            ]))?,
            epoch_version: self.get_latest_epoch()?,
            _h: PhantomData,
        })
    }
}

/////// Helpers //////

pub(crate) async fn get_empty_root<H: Hasher + Send + Sync, S: V2Storage + Send + Sync>(
    storage: &S,
    ep: Option<u64>,
) -> Result<HistoryTreeNode<H>, HistoryTreeNodeError> {
    let mut node = HistoryTreeNode::new(NodeLabel::new(0u64, 0u32), 0, 0, NodeType::Root);
    if let Some(epoch) = ep {
        node.epochs.push(epoch);
        let new_state: HistoryNodeState<H> = HistoryNodeState::new(NodeStateKey(node.label, epoch));
        set_state_map(storage, new_state).await?;
    }

    Ok(node)
}

pub(crate) async fn get_leaf_node<H: Hasher + Sync + Send, S: V2Storage + Sync + Send>(
    storage: &S,
    label: NodeLabel,
    location: usize,
    value: &[u8],
    parent: usize,
    birth_epoch: u64,
) -> Result<HistoryTreeNode<H>, HistoryTreeNodeError> {
    let node = HistoryTreeNode {
        label,
        location,
        epochs: vec![birth_epoch],
        parent,
        node_type: NodeType::Leaf,
        _h: PhantomData,
    };

    let mut new_state: HistoryNodeState<H> =
        HistoryNodeState::new(NodeStateKey(node.label, birth_epoch));
    new_state.value = from_digest::<H>(H::merge(&[H::hash(&[]), H::hash(value)]))?;

    set_state_map(storage, new_state).await?;

    Ok(node)
}

pub(crate) async fn get_leaf_node_without_hashing<
    H: Hasher + Sync + Send,
    S: V2Storage + Sync + Send,
>(
    storage: &S,
    label: NodeLabel,
    location: usize,
    value: H::Digest,
    parent: usize,
    birth_epoch: u64,
) -> Result<HistoryTreeNode<H>, HistoryTreeNodeError> {
    let node = HistoryTreeNode {
        label,
        location,
        epochs: vec![birth_epoch],
        parent,
        node_type: NodeType::Leaf,
        _h: PhantomData,
    };

    let mut new_state: HistoryNodeState<H> =
        HistoryNodeState::new(NodeStateKey(node.label, birth_epoch));
    new_state.value = from_digest::<H>(value)?;

    set_state_map(storage, new_state).await?;

    Ok(node)
}

pub(crate) async fn set_state_map<H: Hasher + Sync + Send, S: V2Storage + Sync + Send>(
    storage: &S,
    val: HistoryNodeState<H>,
) -> Result<(), StorageError> {
    storage.set::<H>(DbRecord::HistoryNodeState(val)).await?;
    Ok(())
}

pub(crate) async fn get_state_map<H: Hasher + Sync + Send, S: V2Storage + Sync + Send>(
    storage: &S,
    node: &HistoryTreeNode<H>,
    key: u64,
) -> Result<HistoryNodeState<H>, StorageError> {
    let record = storage
        .get::<H, HistoryNodeState<H>>(NodeStateKey(node.label, key))
        .await?;
    match record {
        DbRecord::HistoryNodeState(state) => Ok(state),
        _ => Err(StorageError::GetError(String::from("Not found"))),
    }
}
