import json


def load(text):
    return json.loads(text)


def scale(values, factor):
    return [v * factor for v in values]


def summarize(record):
    return record["name"] + ":" + str(len(record["items"]))


data = load('{"name": "x", "items": [1, 2, 3]}')
print(scale([1.0, 2.5], 2.0))
print(summarize({"name": "box", "items": [1, 2]}))
