export function stepSize(meter: string): number {
    return meter === "triplet" ? 1 / 3 : 1 / 4;
}
