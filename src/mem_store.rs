use std::cmp::{Ordering, Ord};
use std::borrow::Borrow;
use std::ops::Bound;

use crate::traits::{AsKey, AsValue, AsNode, Serialize};
use crate::error::BognError;

// TODO: search for red, black and dirty logic and double-check.

/// Llrb to manage a single instance of in-memory sorted index using
/// left-leaning-red-black tree.
///
/// IMPORTANT: This tree is not thread safe.
pub struct Llrb<K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    name: String,
    root: Option<Box<Node<K, V>>>,
    seqno: u64, // seqno so far, starts from 0 and incr for every mutation
    // TODO: llrb_depth_histogram, as feature, to measure the depth of LLRB tree.
}

// TODO: should we implement Drop as part of cleanup
// TODO: Clone trait ?

impl<K, V> Llrb<K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    // create a new instance of Llrb
    pub fn new(name: String, seqno: u64) -> Llrb<K, V> {
        let llrb = Llrb {
            name,
            seqno,
            root: None,
        };
        // TODO: llrb.inittxns()
        llrb
    }

    //    fn load_from<N,K,V>(name: String, iter: Iterator<Item=N>)
    //    where
    //        N: AsNode<K,V>
    //    {
    //        let mut llrb = Llrb::new(name, 0);
    //        for node in iter {
    //            llrb.seqno = node.get_seqno();
    //            if node.is_deleted() {
    //                llrb.delete(node.get_key(), None, true /*lsm*/);
    //            }
    //        }
    //    }

    pub fn id(&self) -> String {
        self.name.clone()
    }

    pub fn set_seqno(&mut self, seqno: u64) {
        self.seqno = seqno;
    }

    pub fn get_seqno(&self) -> u64 {
        self.seqno
    }

    pub fn get<Q>(&self, key: &Q) -> Option<impl AsNode<K,V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut node = &self.root;
        while node.is_some() {
            let nref = node.as_ref().unwrap();
            node = match nref.key.borrow().cmp(key) {
                Ordering::Less => &nref.right,
                Ordering::Equal => return Some(nref.clone_detach()),
                Ordering::Greater => &nref.left,
            };
        }
        None
    }

    pub fn get_versions<Q>(&self, key: &Q) -> Option<impl AsNode<K,V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut node = &self.root;
        while node.is_some() {
            let nref = node.as_ref().unwrap();
            node = match nref.key.borrow().cmp(key) {
                Ordering::Less => &nref.right,
                Ordering::Equal => return Some(*nref.clone()),
                Ordering::Greater => &nref.left,
            };
        }
        None
    }

    pub fn iter(&self) -> Iter<K,V> {
        let mut acc: Vec<Node<K,V>> = vec![];
        let root = &self.root;
        scan(root, &Bound::Unbounded, 100, &mut acc); // TODO: no magic number
        if acc.len() == 0 {
            let after_key = Bound::Unbounded;
            let node_iter = acc.into_iter().rev();
            return Iter{root, empty: true, node_iter, after_key}
        }
        let after_key = Bound::Excluded(acc.last().unwrap().key());
        let node_iter = acc.into_iter().rev();
        return Iter{root, empty: false, node_iter, after_key}
    }

    pub fn set(&mut self, key: K, value: V, lsm: bool) -> Option<impl AsNode<K,V>>
    {
        let seqno = self.seqno + 1;

        let mut res = Llrb::upsert(self.root.take(), key, value, seqno, lsm);
        let mut root = res[0].take().unwrap();
        root.set_black();

        self.root = Some(root);
        self.seqno = seqno;
        match res[1].take() {
            Some(oldnode) => Some(*oldnode),
            None => None,
        }
    }

    fn upsert(
        node: Option<Box<Node<K,V>>>,
        key: K,
        value: V,
        seqno: u64,
        lsm: bool,
        ) -> [Option<Box<Node<K,V>>>; 2]
    {
        if node.is_none() {
            let (access, black) = (0, false);
            [Some(Box::new(Node::new(key, value, seqno, access, black))), None]

        } else {
            let mut node = node.unwrap();
            node = Llrb::walkdown_rot23(node);
            if node.key.gt(&key) {
                let mut res = Llrb::upsert(node.left, key, value, seqno, lsm);
                node.left = res[0].take();
                node = Llrb::walkuprot_23(node);
                [Some(node), res[1].take()]

            } else if node.key.lt(&key) {
                let mut res = Llrb::upsert(node.right, key, value, seqno, lsm);
                node.right = res[0].take();
                node = Llrb::walkuprot_23(node);
                [Some(node), res[1].take()]

            } else {
                let oldnode = node.clone_detach();
                node.prepend_value(value, seqno, 0, /*access*/ lsm);
                node = Llrb::walkuprot_23(node);
                [Some(node), Some(Box::new(oldnode))]
            }
        }
    }

    pub fn set_cas(
        &mut self,
        key: K,
        value: V,
        cas: u64,
        lsm: bool,
        ) -> Result<Option<impl AsNode<K,V>>, BognError>
    {
        let seqno = self.seqno + 1;

        let root = self.root.take();
        let mut res = Llrb::upsert_cas(root, key, value, cas, seqno, lsm)?;
        let mut root = res[0].take().unwrap();
        root.set_black();

        self.root = Some(root);
        self.seqno = seqno;
        match res[1].take() {
            Some(oldnode) => Ok(Some(*oldnode)),
            None => Ok(None),
        }
    }

    fn upsert_cas(
        node: Option<Box<Node<K,V>>>,
        key: K,
        value: V,
        cas: u64,
        seqno: u64,
        lsm: bool,
        ) -> Result<[Option<Box<Node<K,V>>>; 2], BognError>
    {
        if node.is_none() && cas > 0 {
            Err(BognError::InvalidCAS)

        } else if node.is_none() {
            let (access, black) = (0, false);
            let node = Box::new(Node::new(key, value, seqno, access, black));
            Ok([Some(node), None])

        } else {
            let mut node = node.unwrap();
            node = Llrb::walkdown_rot23(node);
            if node.key.gt(&key) {
                let n = node.left;
                let mut res = Llrb::upsert_cas(n, key, value, cas, seqno, lsm)?;
                node.left = res[0].take();
                node = Llrb::walkuprot_23(node);
                Ok([Some(node), res[1].take()])

            } else if node.key.lt(&key) {
                let n = node.right;
                let mut res = Llrb::upsert_cas(n, key, value, cas, seqno, lsm)?;
                node.right = res[0].take();
                node = Llrb::walkuprot_23(node);
                Ok([Some(node), res[1].take()])

            } else if node.is_deleted() && cas != 0 && cas != node.seqno() {
                Err(BognError::InvalidCAS)

            } else if !node.is_deleted() && cas != node.seqno() {
                Err(BognError::InvalidCAS)

            } else {
                let oldnode = node.clone_detach();
                node.prepend_value(value, seqno, 0, /*access*/ lsm);
                node = Llrb::walkuprot_23(node);
                Ok([Some(node), Some(Box::new(oldnode))])
            }
        }
    }

    pub fn delete<Q>(&mut self, key: &Q, lsm: bool) -> Option<impl AsNode<K,V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let seqno = self.seqno + 1;

        let deleted_node = if lsm {
            match self.delete_lsm(key, seqno) {
                res @ Some(_) => res,
                None => {
                    // TODO: handle case were missing key is deleted.
                    None // TODO
                }
            }

        } else {
            let mut res = Llrb::do_delete(self.root.take(), key);
            self.root = res[0].take();
            if self.root.is_some() {
                self.root.as_mut().unwrap().set_black();
            }
            Some(*res[1].take().unwrap())
        };

        self.seqno = seqno;
        deleted_node
    }

    fn delete_lsm<Q>(&mut self, key: &Q, del_seqno: u64) -> Option<Node<K,V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut node = &mut self.root;
        while node.is_some() {
            let nref = node.as_mut().unwrap();
            node = match nref.key.borrow().cmp(key) {
                Ordering::Less => &mut nref.right,
                Ordering::Equal => {
                    nref.delete(del_seqno, true /*true*/);
                    return Some(nref.clone_detach());
                },
                Ordering::Greater => &mut nref.left,
            };
        }
        None
    }

    fn do_delete<Q>(node: Option<Box<Node<K,V>>>, key: &Q)
        -> [Option<Box<Node<K,V>>>; 2]
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if node.is_none() {
            return [None, None];
        }
        let mut node = node.unwrap();
        // TODO: optimize comparision let cmp = node.key.borrow().cmp(key).
        if node.key.borrow().gt(key) {
            if node.left.is_none() {
                return [Some(node), None];
            }
            if !is_red(&node.left) && !is_red(&node.left.as_ref().unwrap().left) {
                node = Llrb::move_red_left(node);
            }
            let mut res = Llrb::do_delete(node.left, key);
            node.left = res[0].take();
            [Some(Llrb::fixup(node)), res[1].take()]

        } else {
            if is_red(&node.left) {
                node = Llrb::rotate_right(node);
            }

            if !node.key.borrow().lt(key) && node.right.is_none() {
                return [None, Some(node)];
            }
            let ok = node.right.is_some() && !is_red(&node.right);
            if ok && !is_red(&node.right.as_ref().unwrap().left) {
                node = Llrb::move_red_right(node);
            }

            if !node.key.borrow().lt(key) { // node == key
                let mut res = Llrb::delete_min(node.right);
                node.right = res[0].take();
                if res[1].is_none() {
                    panic!("do_delete(): fatal logic, call the programmer");
                }
                let mut newnode = node.clone();
                newnode.left = node.left.take();
                node.right = node.right;
                newnode.black = node.black;
                let subdel = res[1].take();
                newnode.valn = subdel.unwrap().valn;
                [Some(Llrb::fixup(newnode)), Some(node)]
            } else {
                let mut res = Llrb::do_delete(node.right, key);
                node.right = res[0].take();
                [Some(Llrb::fixup(node)), res[1].take()]
            }
        }
    }

    fn delete_min(node: Option<Box<Node<K,V>>>) -> [Option<Box<Node<K,V>>>; 2] {
        if node.is_none() {
            return [None, None]
        }
        let mut node = node.unwrap();
        if node.left.is_none() {
            return [None, Some(node)]
        }
        if !is_red(&node.left) && !is_red(&node.left.as_ref().unwrap().left) {
            node = Llrb::move_red_left(node);
        }
        let mut res = Llrb::delete_min(node.left);
        node.left = res[0].take();
        [Some(Llrb::fixup(node)), res[1].take()]
    }

    //--------- rotation routines for 2-3 algorithm ----------------

    fn walkdown_rot23(node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        node
    }

    fn walkuprot_23(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        if is_red(&node.right) && is_black(&node.left) {
            node = Llrb::rotate_left(node);
        }
        if is_red(&node.left) && is_red(&node.left.as_ref().unwrap().left) {
            node = Llrb::rotate_right(node);
        }
        if is_red(&node.left) && is_red(&node.right) {
            node = Llrb::flip(node)
        }
        node
    }

    //              (i)                       (i)
    //               |                         |
    //              node                       x
    //              /  \                      / \
    //             /    (r)                 (r)  \
    //            /       \                 /     \
    //          left       x             node      xr
    //                    / \            /  \
    //                  xl   xr       left   xl
    //
    fn rotate_left(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        if is_black(&node.right) {
            panic!("rotateleft(): rotating a black link ? call the programmer");
        }
        let mut x = node.right.unwrap();
        node.right = x.left;
        x.black = node.black;
        node.set_red();
        x.left = Some(node);
        x
    }

    //              (i)                       (i)
    //               |                         |
    //              node                       x
    //              /  \                      / \
    //            (r)   \                   (r)  \
    //           /       \                 /      \
    //          x       right             xl      node
    //         / \                                / \
    //       xl   xr                             xr  right
    //
    fn rotate_right(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        if is_black(&node.left) {
            panic!("rotateright(): rotating a black link ? call the programmer")
        }
        let mut x = node.left.unwrap();
        node.left = x.right;
        x.black = node.black;
        node.set_red();
        x.right = Some(node);
        x
    }

    //        (x)                   (!x)
    //         |                     |
    //        node                  node
    //        / \                   / \
    //      (y) (z)              (!y) (!z)
    //     /      \              /      \
    //   left    right         left    right
    //
    // REQUIRE: Left and Right children must be present
    fn flip(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        node.left.as_mut().unwrap().toggle_link();
        node.right.as_mut().unwrap().toggle_link();
        node.toggle_link();
        node
    }

    fn fixup(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        if is_red(&node.right) {
            node = Llrb::rotate_left(node);
        }
        if is_red(&node.left) && is_red(&node.left.as_ref().unwrap().left) {
            node = Llrb::rotate_right(node);
        }
        if is_red(&node.left) && is_red(&node.right) {
            node = Llrb::flip(node);
        }
        node
    }

    // REQUIRE: Left and Right children must be present
    fn move_red_left(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        node = Llrb::flip(node);
        if is_red(&node.right.as_ref().unwrap().left) {
            node.right = Some(Llrb::rotate_right(node.right.take().unwrap()));
            node = Llrb::rotate_left(node);
            node = Llrb::flip(node);
        }
        node
    }

    // REQUIRE: Left and Right children must be present
    fn move_red_right(mut node: Box<Node<K, V>>) -> Box<Node<K, V>> {
        node = Llrb::flip(node);
        if is_red(&node.left.as_ref().unwrap().left) {
            node = Llrb::rotate_right(node);
            node = Llrb::flip(node);
        }
        node
    }
}

fn is_red<K, V>(node: &Option<Box<Node<K, V>>>) -> bool
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    if node.is_none() {
        false
    } else {
        !is_black(node)
    }
}

fn is_black<K, V>(node: &Option<Box<Node<K, V>>>) -> bool
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    if node.is_none() {
        true
    } else {
        node.as_ref().unwrap().is_black()
    }
}

//----------------------------------------------------------------------------

#[derive(Clone)]
pub struct ValueNode<V>
where
    V: Default + Clone + Serialize,
{
    data: V,                         // actual value
    seqno: u64,                      // when this version mutated
    deleted: Option<u64>,            // for lsm, deleted > 0
    prev: Option<Box<ValueNode<V>>>, // point to previous version
}

// Various operations on ValueNode, all are immutable operations.
impl<V> ValueNode<V>
where
    V: Default + Clone + Serialize,
{
    fn new(
        data: V,
        seqno: u64,
        deleted: Option<u64>,
        prev: Option<Box<ValueNode<V>>>) -> ValueNode<V>
    {
        let mut vn: ValueNode<V> = Default::default();
        vn.data = data;
        vn.seqno = seqno;
        vn.deleted = deleted;
        vn.prev = prev;
        vn
    }

    fn clone_detach(&self) -> ValueNode<V> {
        ValueNode {
            data: self.data.clone(),
            seqno: self.seqno,
            deleted: self.deleted,
            prev: None
        }
    }

    #[inline]
    fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    fn delete(&mut self, seqno: u64) {
        // back-to-back deletes shall collapse
        self.deleted = Some(seqno);
    }

    fn undo(&mut self) -> bool {
        if self.deleted.is_some() {
            // collapsed deletes can be undone only once
            self.deleted = None;
            true
        } else if self.prev.is_none() {
            false
        } else {
            let source = self.prev.take().unwrap();
            self.clone_from(&source);
            true
        }
    }

    fn value_nodes(&self, acc: &mut Vec<ValueNode<V>>) {
        acc.push(self.clone());
        if self.prev.is_some() {
            self.prev.as_ref().unwrap().value_nodes(acc)
        }
    }
}

impl<V> Default for ValueNode<V>
where
    V: Default + Clone + Serialize,
{
    fn default() -> ValueNode<V> {
        ValueNode {
            data: Default::default(),
            seqno: 0,
            deleted: None,
            prev: None,
        }
    }
}

impl<V> AsValue<V> for ValueNode<V>
where
    V: Default + Clone + Serialize,
{
    fn value(&self) -> V {
        self.data.clone()
    }

    fn seqno(&self) -> u64 {
        self.seqno
    }

    fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }
}

#[derive(Clone)]
pub struct Node<K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    key: K,
    valn: ValueNode<V>,
    access: u64,                    // most recent access for this key
    black: bool,                    // llrb: black or red
    left: Option<Box<Node<K, V>>>,  // llrb: left child
    right: Option<Box<Node<K, V>>>, // llrb: right child
}

// Primary operations on a single node.
impl<K, V> Node<K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    // CREATE operation
    fn new(key: K, value: V, seqno: u64, access: u64, black: bool) -> Node<K, V> {
        let mut node: Node<K, V> = Default::default();
        node.key = key;
        node.valn = ValueNode::new(value, seqno, None, None);
        node.access = access;
        node.black = black;
        node
    }

    fn clone_detach(&self) -> Node<K,V> {
        Node {
            key: self.key.clone(),
            valn: self.valn.clone_detach(),
            access: self.access,
            black: false,
            left: None,
            right: None,
        }
    }

    // prepend operation, equivalent to SET / INSERT / UPDATE
    fn prepend_value(&mut self, value: V, seqno: u64, access: u64, lsm: bool) {
        let prev = if lsm {
            Some(Box::new(self.valn.clone()))
        } else {
            None
        };
        self.valn = ValueNode::new(value, seqno, None, prev);
        self.access = access;
    }

    // DELETE operation
    fn delete(&mut self, seqno: u64, _lsm: bool) {
        self.valn.delete(seqno)
    }

    // UNDO operation
    fn undo(&mut self, lsm: bool) -> bool {
        if lsm {
            self.valn.undo()
        } else {
            false
        }
    }

    #[inline]
    fn set_red(&mut self) {
        self.black = false
    }

    #[inline]
    fn set_black(&mut self) {
        self.black = true
    }

    #[inline]
    fn toggle_link(&mut self) {
        self.black = !self.black
    }

    #[inline]
    fn is_black(&self) -> bool {
        self.black
    }

    //#[inline]
    //fn set_dirty(&mut self, dirty: bool) {
    //    self.dirty = dirty;
    //}

    //#[inline]
    //fn is_dirty(&self) -> bool {
    //    self.dirty
    //}
}

impl<K, V> Default for Node<K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    fn default() -> Node<K, V> {
        Node {
            key: Default::default(),
            valn: Default::default(),
            access: 0,
            black: false,
            left: None,
            right: None,
        }
    }
}

impl<K,V> AsNode<K,V> for Node<K,V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    type Value=ValueNode<V>;

    fn key(&self) -> K {
        self.key.clone()
    }

    fn value(&self) -> Self::Value {
        self.valn.clone()
    }

    fn versions(&self) -> Vec<Self::Value> {
        let mut acc: Vec<Self::Value> = vec![];
        self.valn.value_nodes(&mut acc);
        acc
    }

    fn seqno(&self) -> u64 {
        self.valn.seqno()
    }

    fn access(&self) -> u64 {
        self.access
    }

    fn is_deleted(&self) -> bool {
        self.valn.is_deleted()
    }
}

pub struct Iter<'a, K, V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    empty: bool,
    root: &'a Option<Box<Node<K, V>>>,
    node_iter: std::iter::Rev<std::vec::IntoIter<Node<K,V>>>,
    after_key: Bound<K>,
}

impl<'a,K,V> Iterator for Iter<'a,K,V>
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    type Item=Node<K,V>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.empty {
            return None
        }
        match self.node_iter.next() {
            Some(item) => Some(item),
            None => {
                let mut acc: Vec<Node<K,V>> = vec![];
                scan(self.root, &self.after_key, 100, &mut acc);
                if acc.len() == 0 {
                    self.empty = true;
                    None
                } else {
                    self.after_key = Bound::Excluded(acc.last().unwrap().key());
                    self.node_iter = acc.into_iter().rev();
                    self.node_iter.next()
                }
            }
        }
    }
}

fn scan<K,V>(
    node: &Option<Box<Node<K,V>>>,
    key: &Bound<K>,
    limit: usize,
    acc: &mut Vec<Node<K,V>>) -> bool
where
    K: AsKey,
    V: Default + Clone + Serialize,
{
    if node.is_none() {
        return true
    }
    let node = node.as_ref().unwrap();
    match key {
        Bound::Included(ky) => {
            if node.key.borrow().le(&ky) {
                return scan(&node.right, key, limit, acc)
            }
        },
        Bound::Excluded(ky) => {
            if node.key.borrow().le(&ky) {
                return scan(&node.right, key, limit, acc)
            }
        },
        _ => (),
    }
    if !scan(&node.left, key, limit, acc) {
        return false
    }
    acc.push(node.clone_detach());
    if acc.len() >= limit {
        return false
    }
    return scan(&node.right, key, limit, acc)
}
