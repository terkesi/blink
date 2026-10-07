def calibration_a_display_columns(records):
    widths = {}
    for record in records:
        for key, value in record.items():
            widths[key] = max(widths.get(key, 0), len(str(value)))
    return widths

def calibration_a_dump_rows(records, width):
    return [str(row)[:width] for row in records]
