export function calibration_b_drawColumns(rows: string[][]): number[] {
    const widths: number[] = [];
    for (const row of rows) {
        row.forEach((cell, i) => widths[i] = Math.max(widths[i] ?? 0, cell.length));
    }
    return widths;
}

export function calibration_b_paintRows(rows: string[], width: number): string[] {
    return rows.map(row => row.slice(0, width));
}
