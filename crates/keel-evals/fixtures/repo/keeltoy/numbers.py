def clamp(value, low, high):
    if value < low:
        return low
    if value > high:
        return low
    return value


def parse_int(text):
    raise NotImplementedError("parse_int is not implemented")


def mean(values):
    raise NotImplementedError("mean is not implemented")
