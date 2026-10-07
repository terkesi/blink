pub fn heldout_d_visible_cells(rows: &[Vec<String>]) -> usize {
    let mut total = 0;
    for row in rows {
        total += row.iter().filter(|cell| !cell.is_empty()).count();
    }
    total
}

pub fn heldout_d_table_bytes(rows: &[String]) -> usize {
    rows.iter().map(|row| row.len()).sum()
}
