use std::cmp;
use std::collections::BinaryHeap;
use std::io::Write;
use std::usize;

use {Options, Result};
use file::{File, FileHash};

#[derive(Debug, Clone, Copy)]
enum DiffPrefix {
    None,
    Equal,
    Less,
    Greater,
}

pub(crate) struct PrintState<'a, 'input>
where
    'input: 'a,
{
    indent: usize,
    prefix: DiffPrefix,
    // True if DiffPrefix::Less or DiffPrefix::Greater was printed.
    diff: bool,
    inline_depth: usize,

    // The remaining fields contain information that is commonly needed in print methods.
    pub file: &'a File<'a, 'input>,
    pub hash: &'a FileHash<'a, 'input>,
    pub options: &'a Options<'a>,
}

impl<'a, 'input> PrintState<'a, 'input>
where
    'input: 'a,
{
    pub fn new(
        file: &'a File<'a, 'input>,
        hash: &'a FileHash<'a, 'input>,
        options: &'a Options<'a>,
    ) -> Self {
        PrintState {
            indent: 0,
            prefix: DiffPrefix::None,
            diff: false,
            inline_depth: options.inline_depth,
            file: file,
            hash: hash,
            options: options,
        }
    }

    pub fn indent<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>,
    {
        self.indent += 1;
        let ret = f(self);
        self.indent -= 1;
        ret
    }

    pub fn inline<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>,
    {
        if self.inline_depth == 0 {
            return Ok(());
        }
        self.inline_depth -= 1;
        let ret = f(self);
        self.inline_depth += 1;
        ret
    }


    fn prefix<F>(&mut self, prefix: DiffPrefix, mut f: F) -> Result<()>
    where
        F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>,
    {
        let prev = self.prefix;
        self.prefix = prefix;
        let ret = f(self);
        self.prefix = prev;
        ret
    }

    pub fn line<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Write, &mut PrintState<'a, 'input>) -> Result<()>,
    {
        match self.prefix {
            DiffPrefix::None => {}
            DiffPrefix::Equal => write!(w, "  ")?,
            DiffPrefix::Less => {
                write!(w, "- ")?;
                self.diff = true;
            }
            DiffPrefix::Greater => {
                write!(w, "+ ")?;
                self.diff = true;
            }
        }
        for _ in 0..self.indent {
            write!(w, "\t")?;
        }
        f(w, self)?;
        write!(w, "\n")?;
        Ok(())
    }

    pub fn line_option<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Write, &mut PrintState<'a, 'input>) -> Result<()>,
    {
        let mut buf = Vec::new();
        let mut state = PrintState::new(self.file, self.hash, self.options);
        f(&mut buf, &mut state)?;
        if !buf.is_empty() {
            self.line(w, |w, _state| w.write_all(&*buf).map_err(From::from))?;
        }
        Ok(())
    }

    pub fn line_option_u64(&mut self, w: &mut Write, label: &str, arg: Option<u64>) -> Result<()> {
        if let Some(arg) = arg {
            self.line(w, |w, _state| {
                write!(w, "{}: {}", label, arg)?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn list<T: Print>(
        &mut self,
        label: &str,
        w: &mut Write,
        arg: &T::Arg,
        list: &[T],
    ) -> Result<()> {
        if list.is_empty() {
            return Ok(());
        }

        if !label.is_empty() {
            self.line(w, |w, _state| {
                write!(w, "{}:", label)?;
                Ok(())
            })?;
        }

        self.indent(|state| {
            for item in list {
                item.print(w, state, arg)?;
            }
            Ok(())
        })
    }

    pub fn sort_list<T: SortList>(
        &mut self,
        w: &mut Write,
        arg: &T::Arg,
        list: &mut [&T],
    ) -> Result<()> {
        list.sort_by(|a, b| T::cmp_by(self, a, self, b, self.options));
        for item in list {
            item.print(w, self, arg)?;
        }
        Ok(())
    }
}

pub(crate) struct DiffState<'a, 'input>
where
    'input: 'a,
{
    pub a: PrintState<'a, 'input>,
    pub b: PrintState<'a, 'input>,
    pub options: &'a Options<'a>,
}

impl<'a, 'input> DiffState<'a, 'input>
where
    'input: 'a,
{
    pub fn new(
        file_a: &'a File<'a, 'input>,
        hash_a: &'a FileHash<'a, 'input>,
        file_b: &'a File<'a, 'input>,
        hash_b: &'a FileHash<'a, 'input>,
        options: &'a Options<'a>,
    ) -> Self {
        DiffState {
            a: PrintState::new(file_a, hash_a, options),
            b: PrintState::new(file_b, hash_b, options),
            options: options,
        }
    }

    pub fn diff<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Write, &mut DiffState<'a, 'input>) -> Result<()>,
    {
        let mut buf = Vec::new();
        self.a.diff = false;
        self.b.diff = false;
        f(&mut buf, self)?;
        if self.a.diff || self.b.diff {
            w.write_all(&*buf)?;
        }
        Ok(())
    }

    pub fn ignore_diff<F>(&mut self, flag: bool, mut f: F) -> Result<()>
    where
        F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>,
    {
        let a_diff = self.a.diff;
        let b_diff = self.b.diff;
        f(self)?;
        if flag {
            self.a.diff = a_diff;
            self.b.diff = b_diff;
        }
        Ok(())
    }

    pub fn indent<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>,
    {
        self.a.indent += 1;
        self.b.indent += 1;
        let ret = f(self);
        self.a.indent -= 1;
        self.b.indent -= 1;
        ret
    }

    pub fn inline<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>,
    {
        if self.a.inline_depth == 0 {
            return Ok(());
        }
        self.a.inline_depth -= 1;
        self.b.inline_depth -= 1;
        let ret = f(self);
        self.a.inline_depth += 1;
        self.b.inline_depth += 1;
        ret
    }

    fn prefix_equal<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>,
    {
        let prev_a = self.a.prefix;
        let prev_b = self.b.prefix;
        self.a.prefix = DiffPrefix::Equal;
        self.b.prefix = DiffPrefix::Equal;
        let ret = f(self);
        self.a.prefix = prev_a;
        self.b.prefix = prev_b;
        ret
    }

    fn prefix_less<F>(&mut self, f: F) -> Result<()>
    where
        F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>,
    {
        self.a.prefix(DiffPrefix::Less, f)
    }

    fn prefix_greater<F>(&mut self, f: F) -> Result<()>
    where
        F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>,
    {
        self.b.prefix(DiffPrefix::Greater, f)
    }

    pub fn prefix_diff<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>,
    {
        let prev_a = self.a.prefix;
        let prev_b = self.b.prefix;
        self.a.prefix = DiffPrefix::Less;
        self.b.prefix = DiffPrefix::Greater;
        let ret = f(self);
        self.a.prefix = prev_a;
        self.b.prefix = prev_b;
        ret
    }

    pub fn line<F, T>(&mut self, w: &mut Write, arg_a: T, arg_b: T, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>,
    {
        let mut a = Vec::new();
        let mut state = PrintState::new(self.a.file, self.a.hash, self.a.options);
        f(&mut a, &mut state, arg_a)?;

        let mut b = Vec::new();
        let mut state = PrintState::new(self.b.file, self.b.hash, self.b.options);
        f(&mut b, &mut state, arg_b)?;

        if a == b {
            self.prefix_equal(|state| {
                if !a.is_empty() {
                    state.a.line(w, |w, _state| w.write_all(&*a).map_err(From::from))?;
                }
                Ok(())
            })
        } else {
            self.prefix_diff(|state| {
                if !a.is_empty() {
                    state.a.line(w, |w, _state| w.write_all(&*a).map_err(From::from))?;
                }
                if !b.is_empty() {
                    state.b.line(w, |w, _state| w.write_all(&*b).map_err(From::from))?;
                }
                Ok(())
            })
        }
    }

    /// This is the same as `Self::line`. It exists for symmetry with `PrintState::line_option`.
    pub fn line_option<F, T>(&mut self, w: &mut Write, arg_a: T, arg_b: T, f: F) -> Result<()>
    where
        F: FnMut(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>,
    {
        self.line(w, arg_a, arg_b, f)
    }

    pub fn line_option_u64(
        &mut self,
        w: &mut Write,
        label: &str,
        arg_a: Option<u64>,
        arg_b: Option<u64>,
    ) -> Result<()> {
        let base = arg_a.unwrap_or(0);
        self.line_option(w, arg_a, arg_b, |w, _state, arg| {
            if let Some(arg) = arg {
                write!(w, "{}: {}", label, arg)?;
                if arg != base {
                    write!(w, " ({:+})", arg as i64 - base as i64)?;
                }
            }
            Ok(())
        })
    }

    pub fn list<T: DiffList>(
        &mut self,
        label: &str,
        w: &mut Write,
        arg_a: &T::Arg,
        list_a: &[T],
        arg_b: &T::Arg,
        list_b: &[T],
    ) -> Result<()> {
        if list_a.is_empty() && list_b.is_empty() {
            return Ok(());
        }

        if !label.is_empty() {
            self.line(w, list_a, list_b, |w, _state, list| {
                if !list.is_empty() {
                    write!(w, "{}:", label)?;
                }
                Ok(())
            })?;
        }

        self.indent(|state| {
            let path = shortest_path(
                list_a,
                list_b,
                T::step_cost(),
                |a, b| T::diff_cost(state, arg_a, a, arg_b, b),
            );
            let mut iter_a = list_a.iter();
            let mut iter_b = list_b.iter();
            for dir in path {
                match dir {
                    Direction::None => break,
                    Direction::Diagonal => {
                        if let (Some(a), Some(b)) = (iter_a.next(), iter_b.next()) {
                            T::diff(w, state, arg_a, a, arg_b, b)?;
                        }
                    }
                    Direction::Horizontal => if let Some(a) = iter_a.next() {
                        state.prefix_less(|state| a.print(w, state, arg_a))?;
                    },
                    Direction::Vertical => if let Some(b) = iter_b.next() {
                        state.prefix_greater(|state| b.print(w, state, arg_b))?;
                    },
                }
            }
            Ok(())
        })
    }

    // This is similar to `list`, but because the items are ordered
    // we can do a greedy search.
    //
    // Another significant difference is that items with no difference are not
    // displayed, and added/deleted items are optionally ignored.
    pub fn sort_list<T: SortList>(
        &mut self,
        w: &mut Write,
        arg_a: &T::Arg,
        list_a: &mut [&T],
        arg_b: &T::Arg,
        list_b: &mut [&T],
    ) -> Result<()> {
        list_a.sort_by(|x, y| T::cmp_id_for_sort(&self.a, x, &self.a, y, self.options));
        list_b.sort_by(|x, y| T::cmp_id_for_sort(&self.b, x, &self.b, y, self.options));

        let mut list: Vec<_> = MergeIterator::new(
            list_a.iter(),
            list_b.iter(),
            |a, b| T::cmp_id(&self.a, a, &self.b, b, self.options),
        ).collect();
        list.sort_by(|x, y| {
            MergeResult::cmp(x, y, &self.a, &self.b, |x, state_x, y, state_y| {
                T::cmp_by(state_x, x, state_y, y, self.options)
            })
        });

        for item in list {
            match item {
                MergeResult::Both(a, b) => {
                    self.diff(w, |w, state| T::diff(w, state, arg_a, a, arg_b, b))?;
                }
                MergeResult::Left(a) => if !self.options.ignore_deleted {
                    self.prefix_less(|state| a.print(w, state, arg_a))?;
                },
                MergeResult::Right(b) => if !self.options.ignore_added {
                    self.prefix_greater(|state| b.print(w, state, arg_b))?;
                },
            }
        }
        Ok(())
    }
}

pub(crate) trait Print {
    type Arg;

    // TODO: need associated type constructor to avoid requiring arg to be a reference?
    fn print(&self, w: &mut Write, state: &mut PrintState, arg: &Self::Arg) -> Result<()>;

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        arg_a: &Self::Arg,
        a: &Self,
        arg_b: &Self::Arg,
        b: &Self,
    ) -> Result<()>;
}

impl<'a, T> Print for &'a T
where
    T: Print,
{
    type Arg = T::Arg;

    fn print(&self, w: &mut Write, state: &mut PrintState, arg: &Self::Arg) -> Result<()> {
        T::print(*self, w, state, arg)
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        arg_a: &Self::Arg,
        a: &Self,
        arg_b: &Self::Arg,
        b: &Self,
    ) -> Result<()> {
        T::diff(w, state, arg_a, *a, arg_b, *b)
    }
}

pub(crate) trait DiffList: Print {
    fn step_cost() -> usize;

    fn diff_cost(
        state: &DiffState,
        arg_a: &Self::Arg,
        a: &Self,
        arg_b: &Self::Arg,
        b: &Self,
    ) -> usize;
}

pub(crate) trait SortList: Print {
    fn cmp_id(
        state_a: &PrintState,
        a: &Self,
        state_b: &PrintState,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering;

    fn cmp_id_for_sort(
        state_a: &PrintState,
        a: &Self,
        state_b: &PrintState,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering {
        Self::cmp_id(state_a, a, state_b, b, options)
    }

    fn cmp_by(
        state_a: &PrintState,
        a: &Self,
        state_b: &PrintState,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering;
}

enum MergeResult<T> {
    Left(T),
    Right(T),
    Both(T, T),
}

impl<T> MergeResult<T> {
    fn cmp<Arg, F>(x: &Self, y: &Self, arg_left: Arg, arg_right: Arg, f: F) -> cmp::Ordering
    where
        Arg: Copy,
        F: Fn(&T, Arg, &T, Arg) -> cmp::Ordering,
    {
        match x {
            &MergeResult::Both(ref x_left, _) => match y {
                &MergeResult::Both(ref y_left, _) => f(x_left, arg_left, y_left, arg_left),
                &MergeResult::Left(ref y_left) => f(x_left, arg_left, y_left, arg_left),
                &MergeResult::Right(ref y_right) => f(x_left, arg_left, y_right, arg_right),
            },
            &MergeResult::Left(ref x_left) => match y {
                &MergeResult::Both(ref y_left, _) => f(x_left, arg_left, y_left, arg_left),
                &MergeResult::Left(ref y_left) => f(x_left, arg_left, y_left, arg_left),
                &MergeResult::Right(ref y_right) => f(x_left, arg_left, y_right, arg_right),
            },
            &MergeResult::Right(ref x_right) => match y {
                &MergeResult::Both(ref y_left, _) => f(x_right, arg_right, y_left, arg_left),
                &MergeResult::Left(ref y_left) => f(x_right, arg_right, y_left, arg_left),
                &MergeResult::Right(ref y_right) => f(x_right, arg_right, y_right, arg_right),
            },
        }
    }
}

struct MergeIterator<T, I, C>
where
    T: Copy,
    I: Iterator<Item = T>,
    C: Fn(T, T) -> cmp::Ordering,
{
    iter_left: I,
    iter_right: I,
    item_left: Option<T>,
    item_right: Option<T>,
    item_cmp: C,
}

impl<T, I, C> MergeIterator<T, I, C>
where
    T: Copy,
    I: Iterator<Item = T>,
    C: Fn(T, T) -> cmp::Ordering,
{
    fn new(mut left: I, mut right: I, cmp: C) -> Self {
        let item_left = left.next();
        let item_right = right.next();
        MergeIterator {
            iter_left: left,
            iter_right: right,
            item_left: item_left,
            item_right: item_right,
            item_cmp: cmp,
        }
    }
}

impl<T, I, C> Iterator for MergeIterator<T, I, C>
where
    T: Copy,
    I: Iterator<Item = T>,
    C: Fn(T, T) -> cmp::Ordering,
{
    type Item = MergeResult<T>;

    fn next(&mut self) -> Option<MergeResult<T>> {
        match (self.item_left, self.item_right) {
            (Some(left), Some(right)) => match (self.item_cmp)(left, right) {
                cmp::Ordering::Equal => {
                    self.item_left = self.iter_left.next();
                    self.item_right = self.iter_right.next();
                    Some(MergeResult::Both(left, right))
                }
                cmp::Ordering::Less => {
                    self.item_left = self.iter_left.next();
                    Some(MergeResult::Left(left))
                }
                cmp::Ordering::Greater => {
                    self.item_right = self.iter_right.next();
                    Some(MergeResult::Right(right))
                }
            },
            (Some(left), None) => {
                self.item_left = self.iter_left.next();
                Some(MergeResult::Left(left))
            }
            (None, Some(right)) => {
                self.item_right = self.iter_right.next();
                Some(MergeResult::Right(right))
            }
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    None,
    Diagonal,
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy)]
struct Node {
    cost: usize,
    from: Direction,
    done: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct State {
    cost: usize,
    index1: usize,
    index2: usize,
}

impl Ord for State {
    fn cmp(&self, other: &State) -> cmp::Ordering {
        // min-heap
        other.cost.cmp(&self.cost)
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &State) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// Use Dijkstra's algorithm to find the shortest edit path between two arrays.
//
// step_cost is used for add/delete operations.
//
// diff_cost is used to calculate the cost for modify operations. It should
// return:
//     0 for items that are completely equal
//     (2 * step_cost) for items that are completely different
//     values in between for items that are partially equal
fn shortest_path<Item, F>(
    item1: &[Item],
    item2: &[Item],
    step_cost: usize,
    diff_cost: F,
) -> Vec<Direction>
where
    F: Fn(&Item, &Item) -> usize,
{
    // len + 1 because we need to allow for the 0 items state too.
    let len1 = item1.len() + 1;
    let len2 = item2.len() + 1;
    let len = len1 * len2;

    let mut node: Vec<Node> = vec![
        Node {
            cost: usize::MAX,
            from: Direction::None,
            done: false,
        };
        len
    ];
    let mut heap = BinaryHeap::new();

    fn push(
        node: &mut Node,
        heap: &mut BinaryHeap<State>,
        index1: usize,
        index2: usize,
        cost: usize,
        from: Direction,
    ) {
        if cost < node.cost {
            node.cost = cost;
            node.from = from;
            heap.push(State {
                cost: cost,
                index1: index1,
                index2: index2,
            });
        }
    }

    // Start at the end.  This makes indexing and boundary conditions
    // simpler, and also means we can backtrack in order.
    push(&mut node[len - 1], &mut heap, len1 - 1, len2 - 1, 0, Direction::None);

    while let Some(State { index1, index2, .. }) = heap.pop() {
        if (index1, index2) == (0, 0) {
            let mut result = Vec::new();
            let mut index1 = 0;
            let mut index2 = 0;
            loop {
                let index = index1 + index2 * len1;
                let from = node[index].from;
                match from {
                    Direction::None => {
                        return result;
                    }
                    Direction::Diagonal => {
                        index1 += 1;
                        index2 += 1;
                    }
                    Direction::Horizontal => {
                        index1 += 1;
                    }
                    Direction::Vertical => {
                        index2 += 1;
                    }
                }
                result.push(from);
            }
        }

        let index = index1 + index2 * len1;
        if node[index].done {
            continue;
        }
        node[index].done = true;

        if index2 > 0 {
            let next2 = index2 - 1;
            let next = index - len1;
            if !node[next].done {
                let cost = node[index].cost + step_cost;
                push(&mut node[next], &mut heap, index1, next2, cost, Direction::Vertical);
            }
        }
        if index1 > 0 {
            let next1 = index1 - 1;
            let next = index - 1;
            if !node[next].done {
                let cost = node[index].cost + step_cost;
                push(&mut node[next], &mut heap, next1, index2, cost, Direction::Horizontal);
            }
        }
        if index1 > 0 && index2 > 0 {
            let next1 = index1 - 1;
            let next2 = index2 - 1;
            let next = index - len1 - 1;
            if !node[next].done {
                let diff_cost = diff_cost(&item1[next1], &item2[next2]);
                if diff_cost < 2 * step_cost {
                    let cost = node[index].cost + diff_cost;
                    push(&mut node[next], &mut heap, next1, next2, cost, Direction::Diagonal);
                }
            }
        }
    }

    panic!("failed to find shortest path");
}
