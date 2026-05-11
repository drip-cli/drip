def process_data(items):
    return [item for item in items if item.valid]


def main():
    data = load()
    out = process_data(data)
    print(out)


if __name__ == "__main__":
    main()
