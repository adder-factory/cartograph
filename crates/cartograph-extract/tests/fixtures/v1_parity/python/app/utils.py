import json
import os, sys
import numpy as np


def _private_run():
    return "private"


def run(data: dict) -> str:
    return json.dumps(data)


def compute(values: list) -> float:
    total = 0
    for v in values:
        total += v
    return np.mean(values) if values else total
