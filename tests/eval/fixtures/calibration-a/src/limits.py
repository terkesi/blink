def parcel_band(zone):
    return {"local": 18, "remote": 12}.get(zone, 8)
