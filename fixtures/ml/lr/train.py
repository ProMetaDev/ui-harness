import json
import sys

import numpy as np
from sklearn.datasets import make_classification
from sklearn.linear_model import LogisticRegression
from sklearn.model_selection import train_test_split

seed = int(sys.argv[sys.argv.index("--seed") + 1]) if "--seed" in sys.argv else 0
X, y = make_classification(
    n_samples=400, n_features=8, n_informative=5, random_state=seed
)
Xtr, Xte, ytr, yte = train_test_split(X, y, test_size=0.25, random_state=seed)
model = LogisticRegression(max_iter=500).fit(Xtr, ytr)
acc = float(model.score(Xte, yte))
json.dump(
    {"task": "lr", "seed": seed, "accuracy": round(acc, 4),
     "n_train": int(len(ytr)), "n_test": int(len(yte))},
    open("metrics.json", "w"),
)
print(f"accuracy={acc:.4f}")
