//! Side-by-side layout for overlapping events within one day column.

/// Where one event sits among its overlapping neighbours: its own column and
/// how many columns its cluster of mutually chained overlaps needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) column: usize,
    pub(super) columns: usize,
}

/// Packs `(start, end)` spans, in any unit, into columns. Spans that only
/// touch (one ends exactly when the next starts) do not overlap. Events that
/// chain through a shared neighbour belong to one cluster and share its
/// column count, so widths stay equal inside a cluster. The result is in the
/// input order.
pub(super) fn pack_columns(spans: &[(f32, f32)]) -> Vec<Slot> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&a, &b| {
        spans[a]
            .0
            .total_cmp(&spans[b].0)
            .then(spans[b].1.total_cmp(&spans[a].1))
            .then(a.cmp(&b))
    });

    let mut slots = vec![
        Slot {
            column: 0,
            columns: 1
        };
        spans.len()
    ];
    let mut cluster: Vec<usize> = Vec::new();
    let mut column_ends: Vec<f32> = Vec::new();
    let mut cluster_end = f32::NEG_INFINITY;

    let close = |cluster: &mut Vec<usize>, column_ends: &mut Vec<f32>, slots: &mut [Slot]| {
        for &index in cluster.iter() {
            slots[index].columns = column_ends.len().max(1);
        }
        cluster.clear();
        column_ends.clear();
    };

    for index in order {
        let (start, end) = spans[index];
        if start >= cluster_end {
            close(&mut cluster, &mut column_ends, &mut slots);
            cluster_end = f32::NEG_INFINITY;
        }
        let column = column_ends
            .iter()
            .position(|&column_end| column_end <= start)
            .unwrap_or_else(|| {
                column_ends.push(f32::NEG_INFINITY);
                column_ends.len() - 1
            });
        column_ends[column] = end;
        cluster_end = cluster_end.max(end);
        slots[index].column = column;
        cluster.push(index);
    }
    close(&mut cluster, &mut column_ends, &mut slots);
    slots
}

#[cfg(test)]
mod tests {
    use super::{Slot, pack_columns};

    fn slot(column: usize, columns: usize) -> Slot {
        Slot { column, columns }
    }

    #[test]
    fn separate_events_keep_the_full_width() {
        assert_eq!(
            pack_columns(&[(1.0, 2.0), (3.0, 4.0)]),
            [slot(0, 1), slot(0, 1)]
        );
        assert!(pack_columns(&[]).is_empty());
    }

    #[test]
    fn touching_events_do_not_overlap() {
        assert_eq!(
            pack_columns(&[(1.0, 2.0), (2.0, 3.0)]),
            [slot(0, 1), slot(0, 1)]
        );
    }

    #[test]
    fn two_overlapping_events_sit_side_by_side() {
        assert_eq!(
            pack_columns(&[(1.0, 2.5), (2.0, 3.0)]),
            [slot(0, 2), slot(1, 2)]
        );
    }

    #[test]
    fn identical_times_split_the_width_in_input_order() {
        assert_eq!(
            pack_columns(&[(1.0, 2.0), (1.0, 2.0), (1.0, 2.0)]),
            [slot(0, 3), slot(1, 3), slot(2, 3)]
        );
    }

    #[test]
    fn a_chain_reuses_a_freed_column_but_shares_the_cluster_width() {
        // A overlaps B, B overlaps C, A does not overlap C: C reuses A's column.
        assert_eq!(
            pack_columns(&[(1.0, 2.0), (1.5, 3.5), (2.5, 4.0)]),
            [slot(0, 2), slot(1, 2), slot(0, 2)]
        );
    }

    #[test]
    fn a_long_event_forces_later_short_ones_into_other_columns() {
        assert_eq!(
            pack_columns(&[(1.0, 5.0), (1.5, 2.0), (3.0, 4.0), (6.0, 7.0)]),
            [slot(0, 2), slot(1, 2), slot(1, 2), slot(0, 1)]
        );
    }

    #[test]
    fn the_result_follows_the_input_order_not_the_sorted_order() {
        assert_eq!(
            pack_columns(&[(2.0, 3.0), (1.0, 2.5)]),
            [slot(1, 2), slot(0, 2)]
        );
    }
}
