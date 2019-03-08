use std::ops::Deref;

use crate::traits::{AsDelta, AsEntry, Diff};

/// A single entry in Llrb can have mutiple version of values, DeltaNode
/// represent the difference between this value and next value.
#[derive(Clone, Default)]
pub struct DeltaNode<V>
where
    V: Default + Clone + Diff,
{
    delta: <V as Diff>::D, // actual value
    seqno: u64,            // when this version mutated
    deleted: Option<u64>,  // for lsm, deleted can be > 0
}

// Various operations on DeltaNode, all are immutable operations.
impl<V> DeltaNode<V>
where
    V: Default + Clone + Diff,
{
    fn new(delta: <V as Diff>::D, seqno: u64, deleted: Option<u64>) -> DeltaNode<V> {
        DeltaNode {
            delta,
            seqno,
            deleted,
        }
    }
}

impl<V> AsDelta<V> for DeltaNode<V>
where
    V: Default + Clone + Diff,
{
    #[inline]
    fn delta(&self) -> <V as Diff>::D {
        self.delta.clone()
    }

    #[inline]
    fn seqno(&self) -> u64 {
        self.deleted.map_or(self.seqno, |seqno| seqno)
    }

    #[inline]
    fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }
}

/// Node corresponds to a single entry in Llrb instance.
#[derive(Clone)]
pub struct Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    pub(crate) key: K,
    pub(crate) value: V,
    pub(crate) seqno: u64,
    pub(crate) deleted: Option<u64>,
    pub(crate) deltas: Vec<DeltaNode<V>>,
    pub(crate) black: bool,                    // store: black or red
    pub(crate) dirty: bool,                    // new node in mvcc path
    pub(crate) left: Option<Box<Node<K, V>>>,  // store: left child
    pub(crate) right: Option<Box<Node<K, V>>>, // store: right child
}

// Primary operations on a single node.
impl<K, V> Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    // CREATE operation
    pub(crate) fn new(key: K, value: V, seqno: u64, black: bool) -> Box<Node<K, V>> {
        let node = Box::new(Node {
            key,
            value,
            seqno,
            deleted: None,
            deltas: vec![],
            black,
            dirty: true,
            left: None,
            right: None,
        });
        //println!("new node {:p}", node);
        node
    }

    pub(crate) fn from_entry<E>(entry: E) -> Box<Node<K, V>>
    where
        E: AsEntry<K, V>,
        <E as AsEntry<K, V>>::Delta: Default + Clone,
    {
        let black = false;
        let mut node = Node::new(entry.key(), entry.value(), entry.seqno(), black);
        for delta in entry.deltas().into_iter() {
            let (dt, sq) = (delta.delta(), delta.seqno());
            let dl = if delta.is_deleted() { Some(sq) } else { None };
            node.deltas.push(DeltaNode::new(dt, sq, dl));
        }
        if entry.is_deleted() {
            node.deleted = Some(entry.seqno())
        }
        node
    }

    // unsafe clone for MVCC CoW
    pub(crate) fn mvcc_clone(
        &self,
        reclaim: &mut Vec<Box<Node<K, V>>>, /* reclaim */
    ) -> Box<Node<K, V>> {
        let new_node = Box::new(Node {
            key: self.key.clone(),
            value: self.value.clone(),
            seqno: self.seqno,
            deleted: self.deleted,
            deltas: self.deltas.clone(),
            black: self.black,
            dirty: self.dirty,
            left: self.left_deref().map(|n| n.duplicate()), // TODO: Node::duplicate
            right: self.right_deref().map(|n| n.duplicate()),
        });
        //println!("new node (mvcc) {:p} {:p}", self, new_node);
        reclaim.push(self.duplicate());
        new_node
    }

    #[inline]
    pub(crate) fn left_deref(&self) -> Option<&Node<K, V>> {
        self.left.as_ref().map(|item| item.deref()) // TODO: Box::deref
    }

    #[inline]
    pub(crate) fn right_deref(&self) -> Option<&Node<K, V>> {
        self.right.as_ref().map(|item| item.deref()) // TODO: Box::deref
    }

    // prepend operation, equivalent to SET / INSERT / UPDATE
    pub(crate) fn prepend_version(&mut self, value: V, seqno: u64, lsm: bool) {
        if lsm {
            let delta = self.value.diff(&value);
            let dn = DeltaNode::new(delta, self.seqno, self.deleted);
            self.deltas.push(dn);
            self.value = value;
            self.seqno = seqno;
            self.deleted = None;
        } else {
            self.value = value;
            self.seqno = seqno;
        }
    }

    // DELETE operation
    #[inline]
    pub(crate) fn delete(&mut self, seqno: u64) {
        if self.deleted.is_none() {
            self.deleted = Some(seqno)
        }
    }

    #[inline]
    pub(crate) fn duplicate(&self) -> Box<Node<K, V>> {
        unsafe { Box::from_raw(self as *const Node<K, V> as *mut Node<K, V>) }
    }

    #[inline]
    pub(crate) fn set_red(&mut self) {
        self.black = false
    }

    #[inline]
    pub(crate) fn set_black(&mut self) {
        self.black = true
    }

    #[inline]
    pub(crate) fn toggle_link(&mut self) {
        self.black = !self.black
    }

    #[inline]
    pub(crate) fn is_black(&self) -> bool {
        self.black
    }
}

impl<K, V> Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    // leak nodes children.
    pub(crate) fn mvcc_detach(&mut self) {
        self.left.take().map(|box_node| Box::leak(box_node));
        self.right.take().map(|box_node| Box::leak(box_node));
    }

    // clone and detach this node from the tree.
    pub(crate) fn clone_detach(&self) -> Node<K, V> {
        Node {
            key: self.key.clone(),
            value: self.value.clone(),
            seqno: self.seqno,
            deleted: self.deleted,
            deltas: self.deltas.clone(),
            black: self.black,
            dirty: true,
            left: None,
            right: None,
        }
    }
}

impl<K, V> Default for Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    fn default() -> Node<K, V> {
        Node {
            key: Default::default(),
            value: Default::default(),
            seqno: Default::default(),
            deleted: Default::default(),
            deltas: Default::default(),
            black: false,
            dirty: true,
            left: Default::default(),
            right: Default::default(),
        }
    }
}

impl<K, V> AsEntry<K, V> for Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    type Delta = DeltaNode<V>;

    #[inline]
    fn key(&self) -> K {
        self.key.clone()
    }

    #[inline]
    fn key_ref(&self) -> &K {
        &self.key
    }

    #[inline]
    fn value(&self) -> V {
        self.value.clone()
    }

    #[inline]
    fn seqno(&self) -> u64 {
        self.deleted.map_or(self.seqno, |seqno| seqno)
    }

    #[inline]
    fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    #[inline]
    fn deltas(&self) -> Vec<Self::Delta> {
        self.deltas.clone()
    }
}

/// Fence recursive drops
impl<K, V> Drop for Node<K, V>
where
    K: Default + Clone + Ord,
    V: Default + Clone + Diff,
{
    fn drop(&mut self) {
        self.left.take().map(|left| Box::leak(left));
        self.right.take().map(|right| Box::leak(right));
    }
}