def heldout_b_display_columns(records):
    widths = {}
    for record in records:
        for key, value in record.items():
            widths[key] = max(widths.get(key, 0), len(str(value)))
    return widths

def heldout_b_dump_rows(records, width):
    return [str(row)[:width] for row in records]
