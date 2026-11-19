#!/usr/bin/env python3
"""Mock server for the Fond PWA: serves a trunk `dist` and an in-memory
implementation of the wire contract in `crates/omni-api/src/fond.rs`.

    python3 mock-server.py [--port 8787] [--dist ../dist] [--host 127.0.0.1]
                           [--build mock-1] [--verbose]

Python 3 standard library only. Nothing is persisted; `POST /__mock/reset`
restores the seed.
"""

import argparse
import copy
import hashlib
import json
import queue
import random
import re
import select
import socket
import string
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

HERE = Path(__file__).resolve().parent
GOLDEN = HERE / "../../omni-recipe-format/tests/golden"
FIXTURES = HERE / "fixtures"
# The small sample recipe lives with the preview seed.
PREVIEW = HERE / "../../omni/src/preview_recipes"

UNITS = [
    "batch", "bunch", "can", "clove", "cup", "g", "kg", "l", "lb", "ml", "oz", "piece", "pinch",
    "slice", "sprig", "stalk", "stick", "tbsp", "tsp",
]

# ---------------------------------------------------------------------------
# SEEDED RECIPES: id, title (None = the golden doc's own), golden doc
# ---------------------------------------------------------------------------
SEED_RECIPES = [
    ("rcp_piccata01", "Chicken Piccata", "chicken-piccata"),
    ("rcp_cookies01", "Chocolate Chip Cookies", "chocolate-chip-cookies"),
    ("rcp_enchilada1", None, "enchiladas"),  # "Chicken Enchiladas"
    ("rcp_lasagne001", None, "lasagne-bolognese"),  # "Lasagne alla Bolognese"
    ("rcp_archived01", "Lemon Chicken (old)", "chicken-piccata"),  # archived
    ("rcp_vinaigret1", None, "mustard-vinaigrette"),  # small: five ingredients, three steps
]
# ---------------------------------------------------------------------------

# Fixture photos with their average colour (measured from the JPEGs).
FIXTURE_COLORS = {"dish-a.jpg": "#d5a764", "dish-b.jpg": "#bf9975"}
UPLOAD_COLOR = "#b08968"
RETIRED_EVENTS = ("adjust", "unadjust", "dismissAdjustment")

TAG_GROUPS = [
    ("course", ["main-course", "dessert", "side"]),
    ("cuisine", ["italian", "mexican", "american"]),
    ("main", ["chicken", "beef"]),
    ("method", ["baking"]),
    ("occasion", ["weeknight", "weekend", "make-ahead"]),
]

PICCATA_ORIGINAL_TEXT = "\n".join(
    [
        "Chicken Piccata",
        "Serves 4. Ready in 25 minutes.",
        "",
        "2 large boneless, skinless chicken breasts, halved into cutlets",
        "Salt, pepper and 1/2 cup flour for dredging",
        "2 tbsp olive oil and 4 tbsp cold unsalted butter",
        "1/2 cup dry white wine and 3/4 cup chicken stock",
        "2 lemons, 2 tbsp drained capers, 2 tbsp chopped parsley",
        "",
        "1. Season the cutlets and dredge them in flour, shaking off the excess.",
        "2. Heat the oil with 2 tbsp butter over medium-high heat and sear the cutlets",
        "   until golden, 3 to 4 minutes per side. Move them to a plate.",
        "3. Pour in the wine, scrape up the browned bits and reduce by half.",
        "4. Add the stock, the juice of one lemon and the capers; simmer 3 minutes.",
        "5. Swirl in the remaining butter, return the chicken, top with parsley and lemon slices.",
    ]
)

DAY = 86_400_000
MAX_UPLOAD = 20 * 1024 * 1024
MAX_JSON = 4 * 1024 * 1024
COOK_ID = re.compile(r"^cook_[a-z0-9]{8}$")
ITEM_ID = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
ALNUM = string.ascii_lowercase + string.digits

CONTENT_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript",
    ".css": "text/css",
    ".wasm": "application/wasm",
    ".webmanifest": "application/manifest+json",
    ".png": "image/png",
    ".svg": "image/svg+xml",
    ".ico": "image/x-icon",
    ".json": "application/json",
    ".jpg": "image/jpeg",
    ".woff2": "font/woff2",
    ".txt": "text/plain; charset=utf-8",
}
IMMUTABLE = "public, max-age=31536000, immutable"


def now_ms():
    return int(time.time() * 1000)


def rand_id(prefix, n=8):
    return prefix + "".join(random.choice(ALNUM) for _ in range(n))


def jpeg_size(data):
    """(width, height) from the first SOF segment, or (0, 0)."""
    if data[:2] != b"\xff\xd8":
        return (0, 0)
    i = 2
    while i + 4 <= len(data):
        if data[i] != 0xFF:
            return (0, 0)
        marker = data[i + 1]
        if marker == 0xFF:
            i += 1
            continue
        if marker in (0x01, 0xD8) or 0xD0 <= marker <= 0xD7:
            i += 2
            continue
        length = int.from_bytes(data[i + 2 : i + 4], "big")
        if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
            if i + 9 > len(data):
                return (0, 0)
            height = int.from_bytes(data[i + 5 : i + 7], "big")
            width = int.from_bytes(data[i + 7 : i + 9], "big")
            return (width, height)
        if marker == 0xDA or length < 2:
            return (0, 0)
        i += 2 + length
    return (0, 0)


class ApiError(Exception):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status
        self.message = message


class State:
    """The whole library. Every method takes `self.lock`."""

    def __init__(self, build):
        self.build = build
        self.lock = threading.RLock()
        self.subscribers = []
        self.docs = {
            name: json.loads(
                next(d / f"{name}.json" for d in (GOLDEN, PREVIEW) if (d / f"{name}.json").exists())
                .read_text(encoding="utf-8")
            )
            for name in {golden for _, _, golden in SEED_RECIPES}
        }
        self.fixtures = {name: (FIXTURES / name).read_bytes() for name in FIXTURE_COLORS}
        self.seq = 100
        # A `since` below this is answered in full (set by a reset).
        self.full_before = 0
        self.seed()

    # -- seed ---------------------------------------------------------------

    def seed(self):
        now = now_ms()
        self.images = {}
        self.sources = {}
        self.originals = {}
        self.sessions = {}
        self.photo_uploads = set()
        self.op_results = {}
        self.requests = {}
        self.inbox = {}
        self.inbox_counts = {}

        def image(fixture, image_id):
            data = self.fixtures[fixture]
            digest = hashlib.sha256(data).hexdigest()
            self.images[digest] = data
            width, height = jpeg_size(data)
            return {
                "id": image_id,
                "digest": digest,
                "width": width,
                "height": height,
                "color": FIXTURE_COLORS[fixture],
                "role": "dish",
                "caption": None,
            }

        meta = {
            "rcp_piccata01": dict(
                tags=["main-course", "italian", "chicken", "weeknight"],
                images=[image("dish-a.jpg", "img_dish000a")],
                cover="img_dish000a",
                notes="Double the sauce for guests.",
                notesRev=1,
                activeMin=25,
                totalMin=25,
                cookCount=3,
                lastCookedAt=now - 2 * DAY,
                age=58,
                source={
                    "itemId": "inb_piccata01",
                    "kind": "url",
                    "url": "https://example.com/chicken-piccata",
                    "files": 0,
                },
            ),
            "rcp_cookies01": dict(
                tags=["dessert", "baking", "american"],
                images=[image("dish-b.jpg", "img_dish000b")],
                activeMin=30,
                totalMin=45,
                age=21,
                source={"itemId": "inb_cookies01", "kind": "photo", "url": None, "files": 2},
            ),
            "rcp_enchilada1": dict(
                tags=["main-course", "mexican", "chicken", "baking"],
                activeMin=35,
                totalMin=60,
                cookCount=1,
                lastCookedAt=now - 40 * DAY,
                age=50,
            ),
            "rcp_lasagne001": dict(
                tags=["main-course", "italian", "beef", "make-ahead", "weekend"],
                activeMin=65,
                totalMin=265,
                age=9,
            ),
            "rcp_vinaigret1": dict(
                tags=["side"],
                activeMin=2,
                totalMin=12,
                age=3,
            ),
            "rcp_archived01": dict(
                state="archived",
                tags=["main-course", "chicken"],
                activeMin=25,
                totalMin=25,
                age=60,
            ),
        }

        self.recipes = {}
        for index, (recipe_id, title, golden) in enumerate(SEED_RECIPES):
            m = meta[recipe_id]
            doc = copy.deepcopy(self.docs[golden])
            if title is not None:
                doc["title"] = title
            created = now - m["age"] * DAY
            self.recipes[recipe_id] = {
                "id": recipe_id,
                "revision": 1,
                "format": 1,
                "state": m.get("state", "active"),
                "doc": doc,
                "tags": m["tags"],
                "notes": m.get("notes", ""),
                "notesRev": m.get("notesRev", 0),
                "images": m.get("images", []),
                "cover": m.get("cover"),
                "source": m.get("source"),
                "activeMin": m["activeMin"],
                "totalMin": m["totalMin"],
                "cookCount": m.get("cookCount", 0),
                "lastCookedAt": m.get("lastCookedAt"),
                "createdAt": created,
                "updatedAt": created,
                "seq": index + 1,
            }

        for item_id, kind, title, recipe_id, files in [
            ("inb_cookies01", "photo", "Cookie card", "rcp_cookies01", 2),
            ("inb_piccata01", "url", "Chicken Piccata", "rcp_piccata01", 0),
        ]:
            recipe = self.recipes[recipe_id]
            added = recipe["createdAt"] - 10 * 60_000
            self.inbox[item_id] = {
                "id": item_id,
                "kind": kind,
                "title": title,
                "addedAt": added,
                "files": files,
                "state": "imported",
                "recipeId": recipe_id,
            }
            self.originals[recipe_id] = {
                "itemId": item_id,
                "kind": kind,
                "title": title,
                "url": recipe["source"]["url"],
                "addedAt": added,
                "text": PICCATA_ORIGINAL_TEXT if kind == "url" else None,
                "files": files,
            }
        self.sources["inb_cookies01"] = {
            0: self.fixtures["dish-a.jpg"],
            1: self.fixtures["dish-b.jpg"],
        }

    def reset(self):
        with self.lock:
            # The sequence keeps counting so connected clients cannot mistake
            # the restored library for the one they hold; they resync in full.
            self.seed()
            self.seq += 1
            self.full_before = self.seq
            self.emit_library()
            return {"ok": True, "seq": self.seq}

    # -- events -------------------------------------------------------------

    def subscribe(self):
        q = queue.Queue()
        with self.lock:
            self.subscribers.append(q)
            q.put(frame("hello", {"seq": self.seq, "build": self.build}))
        return q

    def unsubscribe(self, q):
        with self.lock:
            if q in self.subscribers:
                self.subscribers.remove(q)

    def broadcast(self, name, data):
        text = frame(name, data)
        for q in self.subscribers:
            q.put(text)

    def emit_library(self):
        self.broadcast("library", {"seq": self.seq})

    def emit_session(self, session):
        events = session["events"]
        self.broadcast(
            "session", {"id": session["id"], "seq": events[-1]["seq"] if events else 0}
        )

    def touch(self, recipe):
        self.seq += 1
        recipe["seq"] = self.seq
        recipe["updatedAt"] = now_ms()

    # -- sync ---------------------------------------------------------------

    def sync(self, since):
        with self.lock:
            full = since is None or since > self.seq or since < self.full_before
            live = [r for r in self.recipes.values() if r["state"] != "trashed"]
            if full:
                recipes, removed = live, []
            else:
                recipes = [r for r in live if r["seq"] > since]
                removed = [
                    r["id"]
                    for r in self.recipes.values()
                    if r["state"] == "trashed" and r["seq"] > since
                ]
            counts = {}
            for recipe in self.recipes.values():
                if recipe["state"] == "active":
                    for tag in recipe["tags"]:
                        counts[tag] = counts.get(tag, 0) + 1
            tags = [
                {"name": name, "group": group, "description": None, "count": counts.get(name, 0)}
                for group, names in TAG_GROUPS
                for name in names
            ]
            return copy.deepcopy(
                {
                    "seq": self.seq,
                    "full": full,
                    "build": self.build,
                    "liveCount": len(live),
                    "recipes": sorted(recipes, key=lambda r: r["seq"]),
                    "removed": removed,
                    "tags": tags,
                    "sessions": [s for s in self.sessions.values() if s["state"] == "active"],
                    "inboxPending": self.inbox_pending(),
                }
            )

    # -- ops ----------------------------------------------------------------

    def apply_ops(self, body):
        ops = body.get("ops") if isinstance(body, dict) else None
        if not isinstance(ops, list):
            raise ApiError(400, "ops must be an array")
        for op in ops:
            if not isinstance(op, dict) or not isinstance(op.get("id"), str):
                raise ApiError(400, "every op needs an id")
        with self.lock:
            results = []
            changed = False
            asked = False
            for op in ops:
                if op["id"] in self.op_results:
                    results.append(self.op_results[op["id"]])
                    continue
                if op.get("type") == "request":
                    result = self.add_request(op)
                    asked = asked or result["ok"]
                else:
                    result = self.apply_op(op)
                    changed = changed or result["ok"]
                self.op_results[op["id"]] = result
                results.append(result)
            if changed:
                self.emit_library()
            if asked:
                self.broadcast("requests", {})
            return copy.deepcopy({"seq": self.seq, "results": results})

    def add_request(self, op):
        """Stores a request once by its id; a request changes no recipe."""

        def answer(ok, error=None):
            return {"id": op["id"], "ok": ok, "error": error, "recipe": None}

        request_id, text = op.get("requestId"), op.get("text")
        if not isinstance(request_id, str) or not re.fullmatch(r"req_[a-z0-9]{8}", request_id):
            return answer(False, "the request has no valid id")
        if request_id in self.requests:
            return answer(True)
        if not isinstance(text, str) or not text.strip():
            return answer(False, "the request is empty")
        if len(text.strip()) > 2000:
            return answer(False, "a request is limited to 2,000 characters")
        recipe = self.recipes.get(op.get("recipeId")) if op.get("origin") != "library" else None
        if op.get("origin") != "library" and op.get("recipeId") and recipe is None:
            return answer(False, "no such recipe")
        now = now_ms()
        self.requests[request_id] = {
            "id": request_id,
            "text": text.strip(),
            "at": now,
            "updatedAt": now,
            "origin": op.get("origin") if recipe else "library",
            "status": "queued",
            "recipeId": recipe["id"] if recipe else None,
            # Kept for the tests; the app's list does not carry them.
            "context": {
                "revision": op.get("revision") if recipe else None,
                "scale": op.get("scale") if recipe else None,
                "step": op.get("step") if recipe else None,
            },
        }
        return answer(True)

    def request_wire(self, request):
        recipe = self.recipes.get(request.get("recipeId"))
        wire = {k: v for k, v in request.items() if k != "context" and v is not None}
        if recipe is not None:
            wire["recipeTitle"] = recipe["doc"]["title"]
        return wire

    def requests_list(self):
        with self.lock:
            newest = sorted(self.requests.values(), key=lambda r: (-r["at"], r["id"]))
            return copy.deepcopy({"items": [self.request_wire(r) for r in newest]})

    def apply_op(self, op):
        def refuse(error):
            return {"id": op["id"], "ok": False, "error": error, "recipe": None}

        recipe = self.recipes.get(op.get("recipeId"))
        if recipe is None:
            return refuse("unknown recipe")
        kind = op.get("type")
        if kind == "notes":
            text = op.get("text")
            if not isinstance(text, str):
                return refuse("notes op needs text")
            if op.get("baseRev") == recipe["notesRev"] or text == recipe["notes"]:
                recipe["notes"] = text
            else:
                recipe["notes"] = recipe["notes"] + "\n\n" + text
            recipe["notesRev"] += 1
        elif kind == "state":
            if op.get("state") not in ("active", "archived", "trashed"):
                return refuse("unknown state")
            recipe["state"] = op["state"]
        elif kind == "amount":
            error = self.set_amount(recipe, op)
            if error:
                # A refused amount carries the recipe as it is.
                return {"id": op["id"], "ok": False, "error": error, "recipe": copy.deepcopy(recipe)}
        else:
            return refuse("unknown op")
        self.touch(recipe)
        after = None if recipe["state"] == "trashed" else copy.deepcopy(recipe)
        return {"id": op["id"], "ok": True, "error": None, "recipe": after}

    def set_amount(self, recipe, op):
        """One ingredient's quantity and unit, as a new revision. Returns a refusal or None."""
        qty, unit = op.get("qty"), op.get("unit")
        if recipe["state"] == "trashed":
            return "the recipe is in the trash"
        doc = recipe["doc"]
        steps = [step for c in doc["components"] for step in c["steps"]]
        found = [i for c in doc["components"] for i in c["ingredients"] if i["id"] == op.get("ingredientId")]
        if not found:
            return "the recipe no longer has this ingredient"
        ingredient = found[0]
        if ingredient.get("qty") is None:
            return "this ingredient has no amount to change"
        if isinstance(qty, bool) or not isinstance(qty, (int, float)) or not 0 < qty <= 100000:
            return "the amount must be a number above 0, at most 100000"
        if unit is not None and unit not in UNITS and unit != ingredient.get("unit"):
            return "choose one of the listed units"
        sized = [i for step in steps for i in step["inputs"] if i["ref"] == ingredient["id"] and "qty" in i]
        if unit == ingredient.get("unit"):
            if qty == ingredient["qty"]:
                return None
            ratio = qty / ingredient["qty"]
            for used in sized:
                used["qty"] = round(used["qty"] * ratio, 4)
            if "alt" in ingredient:
                ingredient["alt"]["qty"] = round(ingredient["alt"]["qty"] * ratio, 4)
        else:
            if sized:
                return "this ingredient is divided between steps by amount; ask an agent to change its unit"
            ingredient.pop("alt", None)
            if unit is None:
                ingredient.pop("unit", None)
            else:
                ingredient["unit"] = unit
        ingredient["qty"] = qty
        recipe["revision"] += 1
        return None

    # -- cook sessions ------------------------------------------------------

    def start_cook(self, body):
        if not isinstance(body, dict):
            raise ApiError(400, "body must be an object")
        session_id = body.get("id")
        if not isinstance(session_id, str) or not COOK_ID.match(session_id):
            raise ApiError(400, "id must be cook_ plus 8 lowercase letters or digits")
        revision, scale, at = body.get("revision"), body.get("scale"), body.get("at")
        if not is_int(revision) or not is_number(scale) or not is_int(at):
            raise ApiError(400, "revision, scale and at are required numbers")
        with self.lock:
            if session_id in self.sessions:
                return copy.deepcopy(self.sessions[session_id])
            if body.get("recipeId") not in self.recipes:
                raise ApiError(404, "unknown recipe")
            session = {
                "id": session_id,
                "recipeId": body["recipeId"],
                "revision": revision,
                "scale": scale,
                "state": "active",
                "startedAt": at,
                "finishedAt": None,
                "lastEventAt": at,
                "events": [],
                # The pinned revision's document. The mock only keeps heads.
                "doc": copy.deepcopy(self.recipes[body["recipeId"]]["doc"]),
            }
            self.sessions[session_id] = session
            self.emit_session(session)
            return copy.deepcopy(session)

    def get_session(self, session_id):
        with self.lock:
            session = self.sessions.get(session_id)
            if session is None:
                raise ApiError(404, "unknown session")
            return copy.deepcopy(session)

    def append_events(self, session_id, body):
        events = body.get("events") if isinstance(body, dict) else None
        if not isinstance(events, list):
            raise ApiError(400, "events must be an array")
        for event in events:
            if (
                not isinstance(event, dict)
                or not isinstance(event.get("clientEventId"), str)
                or not isinstance(event.get("type"), str)
                or not is_int(event.get("at"))
            ):
                raise ApiError(400, "every event needs clientEventId, at and type")
        with self.lock:
            session = self.sessions.get(session_id)
            if session is None:
                raise ApiError(404, "unknown session")
            appended = False
            library = False
            for event in events:
                if any(e["clientEventId"] == event["clientEventId"] for e in session["events"]):
                    continue
                kind = event["type"]
                # What an app from before overrides were retired may still
                # send: accepted and dropped, like the server does.
                if kind in RETIRED_EVENTS:
                    continue
                if session["state"] != "active":
                    continue
                stored = dict(event)
                stored["seq"] = session["events"][-1]["seq"] + 1 if session["events"] else 1
                stored.setdefault("actor", "app")
                session["events"].append(stored)
                session["lastEventAt"] = event["at"]
                appended = True
                if kind == "finish":
                    self.finish(session, event["at"])
                    library = True
                elif kind == "abandon":
                    # Discarded: closed at once and no cook.
                    session["state"] = "abandoned"
                    session["finishedAt"] = event["at"]
                elif kind == "rebase" and is_int(event.get("revision")):
                    session["revision"] = event["revision"]
                    session["doc"] = copy.deepcopy(self.recipes[session["recipeId"]]["doc"])
            if library:
                self.emit_library()
            if appended:
                self.emit_session(session)
            return copy.deepcopy(session)

    def finish(self, session, at):
        session["state"] = "finished"
        session["finishedAt"] = at
        recipe = self.recipes[session["recipeId"]]
        recipe["cookCount"] += 1
        recipe["lastCookedAt"] = at
        self.touch(recipe)

    def add_photo(self, session_id, data, upload_id=None):
        with self.lock:
            session = self.sessions.get(session_id)
            if session is None:
                raise ApiError(404, "unknown session")
            # `?id=` makes a retried upload store the photo once.
            if upload_id is not None:
                if (session_id, upload_id) in self.photo_uploads:
                    return {"ok": True}
                self.photo_uploads.add((session_id, upload_id))
            digest = hashlib.sha256(data).hexdigest()
            self.images[digest] = data
            width, height = jpeg_size(data)
            recipe = self.recipes[session["recipeId"]]
            recipe["images"].append(
                {
                    "id": rand_id("img_"),
                    "digest": digest,
                    "width": width,
                    "height": height,
                    "color": UPLOAD_COLOR,
                    "role": "dish",
                    "caption": None,
                }
            )
            self.touch(recipe)
            self.emit_library()
            self.emit_session(session)
            return {"ok": True}

    # -- originals, inbox, bytes --------------------------------------------

    def original(self, recipe_id):
        with self.lock:
            recipe = self.recipes.get(recipe_id)
            if recipe is None or recipe["source"] is None or recipe_id not in self.originals:
                raise ApiError(404, "no original")
            return copy.deepcopy(self.originals[recipe_id])

    def inbox_pending(self):
        return sum(
            1
            for item in self.inbox.values()
            if item["state"] == "pending" and item["files"] >= self.inbox_counts.get(item["id"], 0)
        )

    def inbox_list(self):
        with self.lock:
            items = sorted(self.inbox.values(), key=lambda i: i["addedAt"], reverse=True)
            items.sort(key=lambda i: i["state"] != "pending")
            return copy.deepcopy({"items": items[:30]})

    def inbox_upload(self, query, data):
        item_id = query.get("item", [""])[0]
        if not ITEM_ID.match(item_id):
            raise ApiError(400, "item is required")
        try:
            index = int(query["index"][0])
            count = int(query["count"][0])
        except (KeyError, ValueError):
            raise ApiError(400, "index and count are required numbers") from None
        if not (1 <= count <= 50 and 0 <= index < count):
            raise ApiError(400, "index must be below count")
        title = query.get("title", [None])[0] or None
        with self.lock:
            item = self.inbox.get(item_id)
            if item is None:
                item = {
                    "id": item_id,
                    "kind": "photo",
                    "title": title,
                    "addedAt": now_ms(),
                    "files": 0,
                    "state": "pending",
                    "recipeId": None,
                }
                self.inbox[item_id] = item
                self.inbox_counts[item_id] = count
                self.sources[item_id] = {}
            elif item["state"] != "pending" or item["kind"] != "photo":
                raise ApiError(409, "item is not a pending photo item")
            elif self.inbox_counts.get(item_id) != count:
                raise ApiError(400, "count differs from the item's first upload")
            photos = self.sources[item_id]
            was_complete = len(photos) == count
            photos[index] = data
            item["files"] = len(photos)
            if title and not item["title"]:
                item["title"] = title
            complete = len(photos) == count
            if complete and not was_complete:
                self.seq += 1
                self.emit_library()
            return {"itemId": item_id, "received": len(photos), "complete": complete}

    def image(self, digest):
        with self.lock:
            return self.images.get(digest)

    def source_photo(self, item_id, index):
        with self.lock:
            return self.sources.get(item_id, {}).get(index)

    # -- dev helpers --------------------------------------------------------

    def mock_note(self, body):
        for key in ("sessionId", "text"):
            if not isinstance(body.get(key), str):
                raise ApiError(400, "sessionId and text are required")
        event = {
            "seq": 0,
            "clientEventId": rand_id("mock_", 12),
            "at": now_ms(),
            "actor": "agent",
            "type": "note",
            "text": body["text"],
        }
        return self.append_events(body["sessionId"], {"events": [event]})

    def mock_revise(self, body):
        with self.lock:
            recipe = self.recipes.get(body.get("recipeId"))
            if recipe is None:
                raise ApiError(404, "unknown recipe")
            recipe["revision"] += 1
            # A change a reader can see, when asked for one.
            for field in ("summary", "title"):
                if isinstance(body.get(field), str):
                    recipe["doc"][field] = body[field]
            self.touch(recipe)
            self.emit_library()
            return copy.deepcopy(recipe)

    def mock_requests(self, body):
        """The stored requests with their context, oldest first, as an agent lists them."""
        with self.lock:
            status = body.get("status")
            found = [
                r for r in sorted(self.requests.values(), key=lambda r: (r["at"], r["id"]))
                if status in (None, "all")
                or r["status"] == status
                or (status == "open" and r["status"] in ("queued", "working"))
            ]
            return copy.deepcopy({"requests": found})

    def mock_request(self, body):
        """Plays `recipes_request_update`: status, note, resultRevision, resultRecipeId."""
        with self.lock:
            request = self.requests.get(body.get("requestId"))
            if request is None:
                raise ApiError(404, "unknown request")
            before = copy.deepcopy(request)
            if body.get("status") in ("queued", "working", "done", "declined"):
                request["status"] = body["status"]
            if isinstance(body.get("note"), str):
                request["note"] = body["note"].strip() or None
            if is_int(body.get("resultRevision")) or isinstance(body.get("resultRecipeId"), str):
                target = body.get("resultRecipeId") or request.get("recipeId")
                if target not in self.recipes:
                    raise ApiError(404, "unknown recipe")
                request["resultRecipeId"] = target
                request["resultRevision"] = body.get("resultRevision") or self.recipes[target]["revision"]
            if request != before:
                request["updatedAt"] = now_ms()
                self.broadcast("requests", {})
            return copy.deepcopy(request)

    def mock_unimage(self, body):
        """Removes every image of a recipe, as an agent's images edit would."""
        with self.lock:
            recipe = self.recipes.get(body.get("recipeId"))
            if recipe is None:
                raise ApiError(404, "unknown recipe")
            recipe["images"] = []
            recipe["cover"] = None
            self.touch(recipe)
            self.emit_library()
            return copy.deepcopy(recipe)

    def mock_import(self, body):
        with self.lock:
            item = self.inbox.get(body.get("itemId"))
            if item is None:
                raise ApiError(404, "unknown inbox item")
            if body.get("recipeId") not in self.recipes:
                raise ApiError(404, "unknown recipe")
            item["state"] = "imported"
            item["recipeId"] = body["recipeId"]
            self.seq += 1
            self.emit_library()
            return copy.deepcopy(item)


def frame(name, data):
    return f"event: {name}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n"


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "FondMock/1"
    state = None
    dist = None
    verbose = False

    def log_message(self, format, *args):
        if self.verbose:
            sys.stderr.write("%s %s\n" % (self.address_string(), format % args))

    # -- responses ----------------------------------------------------------

    def send_bytes(self, status, content_type, body, cache, head=False):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", cache)
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def send_json(self, status, value, head=False):
        body = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_bytes(status, "application/json", body, "no-store", head)

    def send_error_json(self, status, message, head=False):
        self.send_json(status, {"error": message}, head)

    # -- request bodies -----------------------------------------------------

    def read_body(self, limit):
        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self.close_connection = True
            raise ApiError(411, "content-length required") from None
        if length > limit:
            # The body is left unread, so the connection cannot be reused.
            self.close_connection = True
            raise ApiError(413, "body too large")
        return self.rfile.read(length)

    def read_json(self):
        raw = self.read_body(MAX_JSON)
        try:
            value = json.loads(raw.decode("utf-8")) if raw else {}
        except (ValueError, UnicodeDecodeError):
            raise ApiError(400, "invalid JSON") from None
        return value

    def read_upload(self):
        if self.headers.get("x-fond-upload") is None:
            self.close_connection = True
            raise ApiError(400, "missing x-fond-upload")
        data = self.read_body(MAX_UPLOAD)
        if not data:
            raise ApiError(400, "empty body")
        return data

    # -- routing ------------------------------------------------------------

    def do_GET(self):
        self.route_read(head=False)

    def do_HEAD(self):
        self.route_read(head=True)

    def route_read(self, head):
        url = urlsplit(self.path)
        path = unquote(url.path)
        try:
            if path.startswith("/api/"):
                if head:
                    self.send_error_json(404, "not found", head=True)
                elif path == "/api/events":
                    self.serve_events()
                else:
                    self.send_json(200, self.api_get(path, parse_qs(url.query)))
            elif path.startswith("/img/") or path.startswith("/src/"):
                self.serve_stored(path, head)
            else:
                self.serve_static(path, head)
        except ApiError as error:
            self.send_error_json(error.status, error.message, head)
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    def api_get(self, path, query):
        state = self.state
        if path == "/api/sync":
            since = None
            if "since" in query:
                try:
                    since = int(query["since"][0])
                except ValueError:
                    raise ApiError(400, "since must be a number") from None
            return state.sync(since)
        if path == "/api/inbox":
            return state.inbox_list()
        if path == "/api/requests":
            return state.requests_list()
        parts = path.split("/")[2:]
        if len(parts) == 2 and parts[0] == "cook":
            return state.get_session(parts[1])
        if len(parts) == 2 and parts[0] == "original":
            return state.original(parts[1])
        raise ApiError(404, "not found")

    def do_POST(self):
        url = urlsplit(self.path)
        path = unquote(url.path)
        state = self.state
        try:
            parts = path.strip("/").split("/")
            if path == "/api/ops":
                answer = state.apply_ops(self.read_json())
            elif path == "/api/cook":
                answer = state.start_cook(self.read_json())
            elif len(parts) == 4 and parts[:2] == ["api", "cook"] and parts[3] == "events":
                answer = state.append_events(parts[2], self.read_json())
            elif len(parts) == 4 and parts[:2] == ["api", "cook"] and parts[3] == "photo":
                upload_id = parse_qs(url.query).get("id", [None])[0]
                answer = state.add_photo(parts[2], self.read_upload(), upload_id)
            elif path == "/api/inbox":
                answer = state.inbox_upload(parse_qs(url.query), self.read_upload())
            elif path == "/__mock/reset":
                self.read_json()
                answer = state.reset()
            elif path in (
                "/__mock/note",
                "/__mock/revise",
                "/__mock/import",
                "/__mock/unimage",
                "/__mock/request",
                "/__mock/requests",
            ):
                body = self.read_json()
                if not isinstance(body, dict):
                    raise ApiError(400, "body must be an object")
                answer = getattr(state, "mock_" + parts[1])(body)
            else:
                self.close_connection = True
                raise ApiError(404, "not found")
            self.send_json(200, answer)
        except ApiError as error:
            self.send_error_json(error.status, error.message)
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    # -- stored bytes -------------------------------------------------------

    def serve_stored(self, path, head):
        parts = path.strip("/").split("/")
        data = None
        if len(parts) == 3 and parts[0] == "img" and parts[2] in ("thumb.jpg", "full.jpg"):
            data = self.state.image(parts[1])
        elif len(parts) == 3 and parts[0] == "src" and re.fullmatch(r"\d{1,4}\.jpg", parts[2]):
            data = self.state.source_photo(parts[1], int(parts[2][:-4]))
        if data is None:
            self.send_error_json(404, "not found", head)
        else:
            self.send_bytes(200, "image/jpeg", data, IMMUTABLE, head)

    # -- static files -------------------------------------------------------

    def serve_static(self, path, head):
        spa = (
            path in ("/", "/inbox", "/requests")
            or path.startswith("/r/")
            or path.startswith("/cook/")
        )
        relative = "index.html" if spa else path.lstrip("/")
        segments = relative.split("/")
        if not relative or ".." in segments or "\\" in relative or "\x00" in relative:
            self.send_error_json(404, "not found", head)
            return
        root = self.dist.resolve()
        target = (root / relative).resolve()
        if root not in target.parents or not target.is_file():
            self.send_error_json(404, "not found", head)
            return
        content_type = CONTENT_TYPES.get(target.suffix.lower(), "application/octet-stream")
        cache = IMMUTABLE if path.startswith("/assets/") and not spa else "no-cache"
        self.send_bytes(200, content_type, target.read_bytes(), cache, head)

    # -- server-sent events -------------------------------------------------

    def serve_events(self):
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.send_header("X-Accel-Buffering", "no")
        self.end_headers()
        q = self.state.subscribe()
        last_write = time.monotonic()
        try:
            while not self.server.stopping.is_set():
                try:
                    text = q.get(timeout=0.25)
                except queue.Empty:
                    text = None
                    if self.client_gone():
                        return
                    if time.monotonic() - last_write >= 15:
                        text = ": ping\n\n"
                if text is not None:
                    self.wfile.write(text.encode("utf-8"))
                    self.wfile.flush()
                    last_write = time.monotonic()
        except OSError:
            pass
        finally:
            self.state.unsubscribe(q)

    def client_gone(self):
        """True once the peer closed: an event stream client never sends more."""
        try:
            readable, _, _ = select.select([self.connection], [], [], 0)
            if not readable:
                return False
            return self.connection.recv(1, socket.MSG_PEEK) == b""
        except OSError:
            return True


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    parser = argparse.ArgumentParser(description="Mock server for the Fond PWA.")
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--dist", default="../dist", help="trunk output, relative to this script")
    parser.add_argument("--build", default="mock-1")
    parser.add_argument("--verbose", action="store_true", help="log every request")
    args = parser.parse_args()

    Handler.state = State(args.build)
    Handler.dist = (HERE / args.dist).resolve()
    Handler.verbose = args.verbose
    server = Server((args.host, args.port), Handler)
    server.stopping = threading.Event()
    host = f"[{args.host}]" if ":" in args.host else args.host
    print(f"Fond mock at http://{host}:{server.server_address[1]}/ (dist {Handler.dist})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.stopping.set()
        server.server_close()


if __name__ == "__main__":
    main()
