export function roomLimit(room: string): number {
    return room === "gallery" ? 40 : 12;
}
