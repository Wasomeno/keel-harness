def format_row(cells, widths):
    parts = []
    for cell, width in zip(cells, widths):
        parts.append(cell.ljust(width))
    return " | ".join(parts)


def format_table(headers, rows):
    raise NotImplementedError("format_table is not implemented")
