"""Capture the exact requests pyicloud 2.7.0 sends for Reminders.

A fake session records every POST (path, query, JSON body) and answers with
canned CloudKit replies. Clock and UUIDs are frozen so the output is
deterministic: the Rust port's tests compare against these fixtures.
"""

import base64
import json
import sys
import uuid
import zlib
from datetime import datetime, timezone
from urllib.parse import parse_qsl, urlsplit

from pyicloud.services.reminders import _protocol, _writes
from pyicloud.services.reminders.service import RemindersService

FROZEN_NOW = 1_790_000_000.123  # seconds
FIXED_UUIDS = iter(
    uuid.UUID(f"00000000-0000-4000-8000-{n:012x}") for n in range(1, 1000)
)


class FakeTime:
    @staticmethod
    def time():
        return FROZEN_NOW


_writes.time = FakeTime
_protocol.time = FakeTime
_writes.uuid.uuid4 = lambda: next(FIXED_UUIDS)
_protocol.uuid.uuid4 = lambda: next(FIXED_UUIDS)


def record(name, rtype, fields, tag="tag-1"):
    return {"recordName": name, "recordType": rtype, "fields": fields, "recordChangeTag": tag}


TITLE = _protocol._encode_crdt_document("Comprare il latte")
NOTES = _protocol._encode_crdt_document("Intero, 2 litri")

REMINDER_REC = record(
    "Reminder/AAAA-1",
    "Reminder",
    {
        "TitleDocument": {"type": "STRING", "value": TITLE},
        "NotesDocument": {"type": "ENCRYPTED_BYTES", "value": NOTES},
        "List": {"type": "REFERENCE", "value": {"recordName": "List/L1", "action": "VALIDATE"}},
        "Completed": {"type": "INT64", "value": 0},
        "Priority": {"type": "INT64", "value": 1},
        "Flagged": {"type": "INT64", "value": 1},
        "AllDay": {"type": "INT64", "value": 0},
        "DueDate": {"type": "TIMESTAMP", "value": 1790100000000},
        "CompletionDate": {"type": "TIMESTAMP", "value": -62135596800000},
        "CreationDate": {"type": "TIMESTAMP", "value": 1780000000000},
        "LastModifiedDate": {"type": "TIMESTAMP", "value": 1789000000000},
        "TimeZone": {"type": "STRING", "value": "Europe/Rome"},
        "AlarmIDs": {"type": "STRING_LIST", "value": ["Alarm/X1"]},
        "HashtagIDs": {"type": "STRING_LIST", "value": ["H1"]},
        "ParentReminder": {"type": "REFERENCE", "value": {"recordName": "Reminder/P0"}},
    },
)
LIST_REC = record(
    "List/L1",
    "List",
    {
        "Name": {"type": "STRING", "value": "Spesa"},
        "Color": {"type": "STRING", "value": '{"ckSymbolicColorName":"orange"}'},
        "ReminderIDs": {"type": "STRING", "value": '["Reminder/AAAA-1","BBBB-2"]'},
        "Count": {"type": "INT64", "value": 0},
        "Deleted": {"type": "INT64", "value": 0},
    },
)


class FakeResponse:
    def __init__(self, body):
        self.status_code = 200
        self._body = body
        self.headers = {}
        self.text = json.dumps(body)
        self.content = self.text.encode()

    def json(self):
        return self._body


class FakeSession:
    def __init__(self):
        self.calls = []

    def post(self, url, json=None, timeout=None, headers=None):
        parts = urlsplit(url)
        call = {"path": parts.path, "query": dict(parse_qsl(parts.query)), "body": json}
        self.calls.append(call)
        reply = self._reply(parts, json)
        call["reply"] = reply._body
        return reply

    def _reply(self, parts, json):
        op = parts.path.rsplit("/", 2)[-2:]
        op = "/".join(op)
        if op == "changes/zone":
            zone_req = json["zones"][0]
            types = zone_req.get("desiredRecordTypes")
            recs = []
            if types == ["List"]:
                recs = [LIST_REC]
            elif types == ["Reminder"]:
                recs = [REMINDER_REC, {"recordName": "Reminder/GONE", "deleted": True}]
            return FakeResponse({"zones": [{"zoneID": {"zoneName": "Reminders"}, "records": recs, "syncToken": "TOK-2", "moreComing": False}]})
        if op == "records/query":
            q = json["query"]
            if q["recordType"] == "reminderList":
                return FakeResponse({"records": [REMINDER_REC], "syncToken": "TOK-Q"})
            return FakeResponse({"records": [], "syncToken": "TOK-Q"})
        if op == "records/lookup":
            names = [r["recordName"] for r in json["records"]]
            out = []
            for n in names:
                r = dict(REMINDER_REC)
                r["recordName"] = n
                out.append(r)
            return FakeResponse({"records": out})
        if op == "records/modify":
            out = [{"recordName": o["record"]["recordName"], "recordType": o["record"]["recordType"], "fields": {}, "recordChangeTag": "tag-NEW"} for o in json["operations"]]
            return FakeResponse({"records": out})
        raise AssertionError(f"unexpected call {url}")


def main(out_path):
    session = FakeSession()
    svc = RemindersService(
        "https://p00-ckdatabasews.icloud.com",
        session,
        {"clientBuildNumber": "B", "clientMasteringNumber": "M", "clientId": "CID", "dsid": "42"},
    )
    out = {}

    lists = list(svc.lists())
    out["lists"] = [l.model_dump(mode="json") for l in lists]

    batch = svc.list_reminders("List/L1", include_completed=True, results_limit=200)
    out["list_reminders"] = [r.model_dump(mode="json") for r in batch.reminders]

    out["sync_cursor"] = svc.sync_cursor()
    out["changes"] = [e.model_dump(mode="json") for e in svc.iter_changes(since="TOK-1")]

    created = svc.create(
        "List/L1",
        "Comprare il latte",
        desc="Intero, 2 litri",
        due_date=datetime(2026, 9, 23, 10, 0, tzinfo=timezone.utc),
        priority=1,
        flagged=True,
        time_zone="Europe/Rome",
        parent_reminder_id="P0",
    )
    rem = svc.get("Reminder/AAAA-1")
    rem.title = "Comprare il latte di soia"
    rem.completed = True
    rem.due_date = None
    rem.parent_reminder_id = None
    svc.update(rem)
    svc.delete(rem)
    out["created_id"] = created.id

    out["calls"] = session.calls

    vectors = []
    for text in ["", "Comprare il latte", "Café ☕ 🎉 ok", "a" * 300]:
        enc = _protocol._encode_crdt_document(text)
        doc = zlib.decompress(base64.b64decode(enc))
        vectors.append({"text": text, "document_hex": doc.hex(), "wire": enc})
    out["crdt_vectors"] = vectors

    out["token_map"] = _protocol._generate_resolution_token_map(["titleDocument", "lastModifiedDate"])

    with open(out_path, "w") as f:
        json.dump(out, f, indent=1, ensure_ascii=False)


if __name__ == "__main__":
    main(sys.argv[1])
