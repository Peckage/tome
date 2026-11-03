# xml2json (Python reference version)

The original Python/GTK 3 implementation. The Rust version in the parent folder replaces it for everyday use. This copy is kept as the reference that `../tests/compare_with_python.py` checks the Rust output against.

```sh
python/xml2json-gui [parts.xml]                  # GTK window
python/xml2json-cli parts.xml parts.json [--ndjson] [--tag part]
cd python && python3 -m unittest discover -s tests -v
```
