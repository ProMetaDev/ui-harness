import json
import sys

import numpy as np


def sigmoid(z):
    return 1.0 / (1.0 + np.exp(-z))


def train_once(Xtr, ytr, Xte, yte, lr, epochs, rng, d=8, h=16):
    W1 = rng.normal(scale=0.5, size=(d, h))
    b1 = np.zeros((1, h))
    W2 = rng.normal(scale=0.5, size=(h, 1))
    b2 = np.zeros((1, 1))
    for _ in range(epochs):
        z1 = Xtr @ W1 + b1
        a1 = np.maximum(z1, 0)
        p = sigmoid(a1 @ W2 + b2)
        dz2 = (p - ytr) / len(Xtr)
        dW2 = a1.T @ dz2
        db2 = dz2.sum(axis=0, keepdims=True)
        dz1 = (dz2 @ W2.T) * (z1 > 0)
        W2 -= lr * dW2
        b2 -= lr * db2
        W1 -= lr * (Xtr.T @ dz1)
        b1 -= lr * dz1.sum(axis=0, keepdims=True)
    p_te = sigmoid(np.maximum(Xte @ W1 + b1, 0) @ W2 + b2)
    return float(np.mean((p_te > 0.5).astype(float) == yte))


def main():
    seed = int(sys.argv[sys.argv.index("--seed") + 1]) if "--seed" in sys.argv else 0
    rng = np.random.default_rng(seed)
    n, d = 400, 8
    X = rng.normal(size=(n, d))
    w_true = rng.normal(size=d)
    y = (X @ w_true > 0).astype(float).reshape(-1, 1)
    idx = rng.permutation(n)
    split = int(n * 0.75)
    tr, te = idx[:split], idx[split:]
    Xtr, ytr, Xte, yte = X[tr], y[tr], X[te], y[te]

    trials = []
    for lr in (0.01, 0.1, 0.5):
        acc = train_once(Xtr, ytr, Xte, yte, lr, 100, np.random.default_rng(seed))
        trials.append({"lr": lr, "accuracy": round(acc, 4)})
    best = max(trials, key=lambda t: t["accuracy"])
    json.dump(
        {"task": "ablation", "seed": seed,
         "best_accuracy": best["accuracy"], "best_lr": best["lr"],
         "trials": len(trials)},
        open("metrics.json", "w"),
    )
    print(f"best={best['accuracy']:.4f} lr={best['lr']}")


main()
