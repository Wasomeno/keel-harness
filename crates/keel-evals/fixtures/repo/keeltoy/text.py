def normalize_space(text):
    return " ".join(text.split())


def dedupe_whitespace(text):
    return " ".join(text.split())


def truncate(text, limit):
    if len(text) <= limit:
        return text
    return text[:limit] + "…"


def slugify(text):
    raise NotImplementedError("slugify is not implemented")
