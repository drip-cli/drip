def process_data(items):
    return [item for item in items if item.is_valid()]


def main():
    setup_logging()
    data = load()
    out = process_data(data)
    print(out)


if __name__ == "__main__":
    main()
