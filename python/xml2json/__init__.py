"""Stream large XML files to JSON."""

from .core import Cancelled, convert, count_lines, guess_record_tag

__all__ = ["Cancelled", "convert", "count_lines", "guess_record_tag"]
__version__ = "1.0.0"
