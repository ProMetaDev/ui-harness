import json
import sys

import numpy as np


def sigmoid(z):
    return 1.0 / (1.0 + np.exp(-z))


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

    h = 16
    W1 = rng.normal(scale=0.5, size=(d, h))
    b1 = np.zeros((1, h))
    W2 = rng.normal(scale=0.5, size=(h, 1))
    b2 = np.zeros((1, 1))
    lr, epochs = 0.1, 120

    loss = 0.0
    for _ in range(epochs):
        z1 = Xtr @ W1 + b1
        a1 = np.maximum(z1, 0)
        z2 = a1 @ W2 + b2
        p = sigmoid(z2)
        eps = 1e-9
        loss = float(-np.mean(ytr * np.log(p + eps) + (1 - ytr) * np.log(1 - p + eps)))
        dz2 = (p - ytr) / len(Xtr)
        dW2 = a1.T @ dz2
        db2 = dz2.sum(axis=0, keepdims=True)
        da1 = dz2 @ W2.T
        dz1 = da1 * (z1 > 0)
        dW1 = Xtr.T @ dz1
        db1 = dz1.sum(axis=0, keepdims=True)
        W2 -= lr * dW2
        b2 -= lr * db2
        W1 -= lr * dW1
        b1 -= lr * db1

    p_te = sigmoid(np.maximum(Xte @ W1 + b1, 0) @ W2 + b2)
    acc = float(np.mean((p_te > 0.5).astype(float) == yte))
    json.dump(
        {"task": "mlp", "seed": seed, "accuracy": round(acc, 4),
         "final_loss": round(loss, 4), "hidden": h, "epochs": epochs},
        open("metrics.json", "w"),
    )
    print(f"accuracy={acc:.4f} loss={loss:.4f}")


main()
