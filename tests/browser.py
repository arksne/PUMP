"""Проверяет плеер по HTTP в двух независимых мобильных сессиях Chromium."""

import json
import hashlib
import sqlite3
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
import re
from pathlib import Path
import shutil
import socket
import subprocess
from tempfile import TemporaryDirectory
import time
import unittest
from urllib.request import Request, urlopen
from urllib.error import HTTPError
from provider_fixture import module as provider_module
from uuid import uuid4

from playwright.sync_api import expect, sync_playwright


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("PUMP_TEST_BINARY", ROOT / "target/debug/pump")).resolve()


def make_audio(path, frequency, title):
    subprocess.run(
        [
            "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", f"sine=frequency={frequency}:duration=40",
            "-b:a", "96k", "-metadata", f"title={title}", "-metadata",
            "artist=PUMP Test Artist", str(path),
        ],
        check=True,
    )


def wait_for_audio(page, predicate):
    deadline = time.monotonic() + 8
    audio = page.locator("#audio")
    while time.monotonic() < deadline:
        if audio.evaluate(predicate):
            return
        page.wait_for_timeout(100)
    state = audio.evaluate("(audio) => ({position: audio.currentTime, paused: audio.paused})")
    raise AssertionError(f"Плеер не перешёл в ожидаемое состояние: {predicate}, {state}")


def soundcloud_stub(media, entries, client_id="a"*32):
    """Локальный api-v2 + CDN: поиск, resolve и поток без внешней сети.
    entries: [{"id": "424242", "title": ..., "artist": ...}].
    Подсчёт обращений к resolve — через handler.calls."""
    def track_json(entry, base):
        return {"id":int(entry["id"]),"title":entry["title"],"user":{"username":entry["artist"]},
                "duration":40000,"full_duration":40000,"policy":"ALLOW",
                "media":{"transcodings":[{"url":base+f"/media/soundcloud:tracks:{entry['id']}/abc/stream/progressive",
                                          "format":{"protocol":"progressive","mime_type":"audio/mpeg"}}]}}
    class ScHandler(BaseHTTPRequestHandler):
        calls=[]
        def payload(handler,obj,status=200):
            body=json.dumps(obj).encode()
            handler.send_response(status)
            handler.send_header("Content-Type","application/json")
            handler.send_header("Content-Length",str(len(body)))
            handler.end_headers()
            handler.wfile.write(body)
        def do_GET(handler):
            base=f"http://127.0.0.1:{handler.server.server_port}"
            by_id={e["id"]:e for e in entries}
            if handler.path.startswith("/search?"):
                assert "client_id="+client_id in handler.path
                handler.payload({"collection":[track_json(by_id[i],base) for i in sorted(by_id)]})
            elif handler.path.startswith("/tracks/"):
                ident=handler.path.split("/tracks/")[1].split("?")[0]
                if ident in by_id:
                    handler.payload(track_json(by_id[ident],base))
                else:
                    handler.send_error(404)
            elif "/stream/progressive" in handler.path:
                assert "client_id="+client_id in handler.path
                type(handler).calls.append(handler.path)
                handler.payload({"url":base+"/cdn/track.mp3"})
            elif handler.path.startswith("/cdn/track.mp3"):
                start,end=0,len(media)-1
                if value:=handler.headers.get("Range"):
                    found=re.fullmatch(r"bytes=(\d+)-(\d*)",value)
                    if not found:
                        handler.send_error(416)
                        return
                    start=int(found[1]);end=min(int(found[2]) if found[2] else end,end)
                handler.send_response(206 if handler.headers.get("Range") else 200)
                handler.send_header("Content-Type","audio/mpeg")
                handler.send_header("Content-Length",str(end-start+1))
                if handler.headers.get("Range"):
                    handler.send_header("Content-Range",f"bytes {start}-{end}/{len(media)}")
                handler.end_headers()
                handler.wfile.write(media[start:end+1])
            else:
                handler.send_error(404)
        def log_message(self,*args):
            pass
    return ScHandler


def audius_stub(media):
    """Official API shape, public/gated metadata, redirect and byte ranges."""
    def metadata(ident):
        return {"id": ident, "title": "Audius Test " + ident, "user": {"name": "Fixture Artist"},
                "duration": 40, "is_streamable": True, "is_stream_gated": ident == "Gated1"}
    class Handler(BaseHTTPRequestHandler):
        def payload(self, data):
            body=json.dumps({"data":data}).encode()
            self.send_response(200)
            self.send_header("Content-Type","application/json")
            self.send_header("Content-Length",str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_GET(self):
            path=self.path.split("?")[0]
            if path=="/v1/tracks/search":
                self.payload([metadata("Public1"),metadata("Gated1")])
            elif path in ("/v1/tracks/Public1","/v1/tracks/Gated1","/v1/tracks/Unsafe1"):
                self.payload(metadata(path.rsplit("/",1)[1]))
            elif path=="/v1/tracks/Public1/stream":
                if "preview=false" not in self.path:
                    self.send_error(400)
                    return
                self.send_response(302)
                self.send_header("Location","/cdn/public.mp3")
                self.end_headers()
            elif path=="/v1/tracks/Unsafe1/stream":
                self.send_response(302)
                self.send_header("Location","http://127.0.0.1:1/private")
                self.end_headers()
            elif path=="/cdn/public.mp3":
                start,end=0,len(media)-1
                value=self.headers.get("Range")
                if value:
                    found=re.fullmatch(r"bytes=(\d+)-(\d*)",value)
                    if not found:
                        self.send_error(416)
                        return
                    start=int(found[1]);end=min(int(found[2]) if found[2] else end,end)
                    if start>end:
                        self.send_response(416)
                        self.send_header("Content-Range",f"bytes */{len(media)}")
                        self.end_headers()
                        return
                self.send_response(206 if value else 200)
                self.send_header("Content-Type","audio/mpeg")
                self.send_header("Content-Length",str(end-start+1))
                self.send_header("Accept-Ranges","bytes")
                if value:
                    self.send_header("Content-Range",f"bytes {start}-{end}/{len(media)}")
                self.end_headers()
                self.wfile.write(media[start:end+1])
            else:
                self.send_error(404)
        def log_message(self,*args):
            pass
    return Handler


class PlayerTest(unittest.TestCase):
    def setUp(self):
        self.folder = TemporaryDirectory(prefix="pump-browser-")
        self.addCleanup(self.folder.cleanup)
        self.root = Path(self.folder.name)
        music = self.root / "music"
        music.mkdir()
        make_audio(music / "alpha.mp3", 440, "North Loop")
        make_audio(music / "beta.mp3", 550, "South Loop")
        with socket.socket() as chosen:
            chosen.bind(("127.0.0.1", 0))
            port = chosen.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        self.environment = {
            **os.environ, "BIND_ADDR": "127.0.0.1", "PORT": str(port),
            "PUMP_MEDIA_DIR": str(music), "PUMP_DATA_DIR": str(self.root / "data"),
        }
        for setting in ("TELOXIDE_TOKEN", "TELEGRAM_BOT_TOKEN", "WEBAPP_URL", "YOUTUBE_API_KEY"):
            self.environment.pop(setting, None)
        log = (self.root / "pump.log").open("w+")
        self.addCleanup(log.close)
        self.log = log
        self.start()
        self.addCleanup(self.stop)

    def start(self):
        self.server = subprocess.Popen(
            [str(BINARY)], cwd=self.root, env=self.environment, stdout=self.log, stderr=self.log
        )
        for _ in range(100):
            try:
                status, _, _ = self.request("/healthz")
                if status == 200:
                    return
            except OSError:
                if self.server.poll() is not None:
                    break
            time.sleep(0.05)
        self.stop()
        self.log.seek(0)
        self.fail(f"Сервер не запустился. {self.log.read()}")

    def stop(self):
        if self.server.poll() is None:
            self.server.terminate()
            try:
                self.server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait(timeout=5)

    def request(self, path, *, body=None, headers=None, timeout=12):
        headers = dict(headers or {})
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body).encode()
        with urlopen(Request(self.origin + path, data=body, headers=headers), timeout=timeout) as response:
            return response.status, response.headers, response.read()

    def test_catalog_and_persistent_statistics(self):
        _, _, data = self.request("/api/search?source=full&q=North")
        [track] = json.loads(data)["tracks"]
        self.assertEqual(track["kind"], "full")
        self.assertEqual(track["artist"], "PUMP Test Artist")
        self.assertGreater(track["duration_ms"], 39_000)
        status, headers, data = self.request(track["audio_url"], headers={"Range": "bytes=100-149"})
        self.assertEqual(status, 206)
        self.assertEqual(headers.get_content_type(), "audio/mpeg")
        self.assertEqual(len(data), 50)

        _, _, data = self.request("/api/search?source=youtube&q=https%3A%2F%2Fyoutu.be%2F5NV6Rdv1a3I")
        [video] = json.loads(data)["tracks"]
        self.assertEqual((video["kind"], video["video_id"], video["audio_url"]), ("youtube", "5NV6Rdv1a3I", None))

        _, _, session = self.request("/api/session", body={})
        auth = {"Authorization": "Bearer " + json.loads(session)["token"]}
        event = {"track_id": track["id"], "event_id": str(uuid4()), "milliseconds": 1200}
        for _ in range(2):
            status, _, _ = self.request("/api/listens", headers=auth, body=event)
            self.assertEqual(status, 204)
        self.stop()
        self.start()
        _, _, data = self.request("/api/stats?period=month", headers=auth)
        stats = json.loads(data)
        self.assertEqual((stats["tracks"], stats["milliseconds"]), (1, 1200))
        self.assertEqual(stats["top_artists"][0]["title"], track["artist"])

    def test_audius_public_search_stream_range_and_gates(self):
        media=(self.root/"music/alpha.mp3").read_bytes()
        stub=ThreadingHTTPServer(("127.0.0.1",0),audius_stub(media))
        threading.Thread(target=stub.serve_forever,daemon=True).start()
        self.addCleanup(stub.server_close)
        self.addCleanup(stub.shutdown)
        self.stop()
        self.environment["PUMP_AUDIUS_TEST_API_BASE"]=f"http://127.0.0.1:{stub.server_port}/v1"
        self.start()
        _,_,data=self.request("/api/search?source=audius&q=Fixture")
        tracks=json.loads(data)["tracks"]
        self.assertEqual([t["id"] for t in tracks],["au-Public1"])
        self.assertEqual((tracks[0]["source"],tracks[0]["kind"],tracks[0]["duration_ms"]),("Audius","full",40000))
        status,headers,part=self.request(tracks[0]["audio_url"],headers={"Range":"bytes=100-199"})
        self.assertEqual((status,part),(206,media[100:200]))
        self.assertEqual(headers["Content-Range"],f"bytes 100-199/{len(media)}")
        with self.assertRaises(HTTPError) as gated:
            self.request("/source/au/Gated1")
        self.assertEqual(gated.exception.code,409)
        with self.assertRaises(HTTPError) as unsafe:
            self.request("/source/au/Unsafe1")
        self.assertEqual(unsafe.exception.code,502)
        with self.assertRaises(HTTPError) as invalid:
            self.request("/source/au/Public1",headers={"Range":"bytes=0-1,4-5"})
        self.assertEqual(invalid.exception.code,416)
        _,_,metadata=self.request("/api/tracks/au-Public1")
        self.assertEqual(json.loads(metadata)["id"],"au-Public1")
        with sync_playwright() as driver:
            executable=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser=driver.chromium.launch(executable_path=executable)
            page=browser.new_page(viewport={"width":390,"height":844})
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("audius")
            page.locator("#qInput").fill("Fixture")
            page.get_by_role("button",name="Audius Test Public1",exact=False).click()
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > 0.5")
            page.locator("#seekBar").evaluate("(bar) => { bar.value = 50; bar.dispatchEvent(new Event('input', {bubbles: true})); }")
            wait_for_audio(page,"audio => audio.currentTime >= 19.5")
            self.assertEqual(errors,[])
            browser.close()

    def test_external_sources_shape_and_guards(self):
        _, _, data = self.request("/api/search?source=spotify&q=North")
        body = json.loads(data)
        self.assertEqual(body["tracks"], [])
        self.assertIn("SPOTIFY_CLIENT", body["notice"])
        with self.assertRaises(HTTPError) as invalid:
            self.request("/source/sc/abc")
        self.assertEqual(invalid.exception.code, 404)
        tracks = self.soundcloud_live_tracks("North")
        for track in tracks:
            self.assertTrue(track["id"].startswith("sc-"))
            self.assertTrue(track["audio_url"].startswith("/source/sc/"))
            self.assertEqual((track["kind"], track["source"]), ("full", "SoundCloud"))

    def soundcloud_live_tracks(self, query):
        """Живой поиск SoundCloud: при недоступности сети — skip,
        при пустом ответе без пометки о недоступности — провал (регрессия)."""
        _, _, data = self.request(f"/api/search?source=soundcloud&q={query}")
        body = json.loads(data)
        tracks = [t for t in body["tracks"] if t["kind"] == "full"]
        if not tracks and body.get("notice") and "недоступен" in body["notice"]:
            self.skipTest("SoundCloud недоступен из этой сети")
        return tracks

    def test_soundcloud_stub_chain_search_resolve_stream(self):
        media=(self.root/"music/alpha.mp3").read_bytes()
        entries=[{"id":"424242","title":"Stub Loop","artist":"Stub Artist"},
                 {"id":"434343","title":"Second Stub","artist":"Stub Artist"}]
        ScHandler=soundcloud_stub(media,entries)
        stub=ThreadingHTTPServer(("127.0.0.1",0),ScHandler)
        threading.Thread(target=stub.serve_forever,daemon=True).start()
        self.addCleanup(stub.server_close)
        self.addCleanup(stub.shutdown)
        self.stop()
        self.environment.update(SOUNDCLOUD_CLIENT_ID="a"*32,SOUNDCLOUD_API_BASE=f"http://127.0.0.1:{stub.server_port}")
        self.start()
        _,_,data=self.request("/api/search?source=soundcloud&q=Stub")
        found={t["id"]:t for t in json.loads(data)["tracks"]}
        self.assertEqual(set(found),{"sc-424242","sc-434343"})
        # The default search must expose full remote recordings, not just previews.
        _,_,combined=self.request("/api/search?source=all&q=Stub")
        combined_tracks=json.loads(combined)["tracks"]
        self.assertTrue(any(t["id"]=="sc-424242" and t["kind"]=="full" for t in combined_tracks))
        ids=[t["id"] for t in combined_tracks]
        self.assertEqual(len(ids),len(set(ids)))
        self.assertEqual(ids.count("sc-424242"),1)
        track=found["sc-424242"]
        self.assertEqual((track["title"],track["source"]),("Stub Loop","SoundCloud"))
        self.assertEqual(track["audio_url"],"/source/sc/424242")
        status,headers,part=self.request(track["audio_url"],headers={"Range":"bytes=0-99"})
        self.assertEqual((status,part),(206,media[:100]))
        self.assertTrue(headers["Content-Range"].startswith("bytes 0-99/"))
        # Каждый запрос перерезолвит подпись заново: кэша подписанных URL нет.
        before=len(ScHandler.calls)
        status,_,again=self.request(track["audio_url"],headers={"Range":"bytes=100-199"})
        self.assertEqual((status,again),(206,media[100:200]))
        self.assertGreater(len(ScHandler.calls),before)
        _,_,meta=self.request("/api/tracks/"+track["id"])
        self.assertEqual(json.loads(meta)["title"],"Stub Loop")
        with sync_playwright() as driver:
            executable=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser=driver.chromium.launch(executable_path=executable)
            page=browser.new_page(viewport={"width":390,"height":844})
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("soundcloud")
            page.locator("#qInput").fill("Stub")
            page.get_by_role("button",name="Stub Loop",exact=False).click()
            expect(page.locator("#tTitle")).to_have_text("Stub Loop")
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > 0.5")
            # Перемотка внутри проксированного потока.
            page.locator("#seekBar").evaluate("(bar) => { bar.value = 50; bar.dispatchEvent(new Event('input', {bubbles: true})); }")
            wait_for_audio(page,"audio => audio.currentTime >= 19.5")
            # Смена трека: второй результат играет с начала.
            page.locator("#tabSearch").click()
            page.get_by_role("button",name="Second Stub",exact=False).click()
            expect(page.locator("#tTitle")).to_have_text("Second Stub")
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime < 5 && audio.currentTime > 0.2")
            self.assertEqual(errors,[])
            browser.close()

    def test_soundcloud_stub_room_follows_host(self):
        media=(self.root/"music/alpha.mp3").read_bytes()
        entries=[{"id":"424242","title":"Stub Loop","artist":"Stub Artist"}]
        ScHandler=soundcloud_stub(media,entries)
        stub=ThreadingHTTPServer(("127.0.0.1",0),ScHandler)
        threading.Thread(target=stub.serve_forever,daemon=True).start()
        self.addCleanup(stub.server_close)
        self.addCleanup(stub.shutdown)
        self.stop()
        self.environment.update(SOUNDCLOUD_CLIENT_ID="a"*32,SOUNDCLOUD_API_BASE=f"http://127.0.0.1:{stub.server_port}")
        self.start()
        with sync_playwright() as driver:
            executable=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser=driver.chromium.launch(executable_path=executable)
            device=driver.devices["Pixel 7"]
            host=browser.new_context(**device).new_page()
            guest=browser.new_context(**device).new_page()
            errors=[]
            for page in (host,guest):
                page.on("pageerror",lambda error:errors.append(str(error)))
                page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
                page.goto(self.origin)
            host.locator("#tabSearch").click()
            host.locator("#source").select_option("soundcloud")
            host.locator("#qInput").fill("Stub")
            host.get_by_role("button",name="Stub Loop",exact=False).click()
            expect(host.locator("#tTitle")).to_have_text("Stub Loop")
            wait_for_audio(host,"audio => !audio.paused && audio.currentTime > 0.5")
            host.locator("#tabRooms").click()
            host.locator("#createRoom").click()
            expect(host.locator("#roomStatus")).to_contain_text("Ведущий")
            code=host.locator("#roomCode").input_value()
            guest.locator("#tabRooms").click()
            guest.locator("#roomCode").fill(code)
            guest.get_by_role("button",name="Войти",exact=True).click()
            expect(guest.locator("#roomStatus")).to_contain_text("Слушатель")
            guest.locator("#enableRoomSound").click()
            expect(guest.locator("#tTitle")).to_have_text("Stub Loop")
            wait_for_audio(guest,"audio => !audio.paused && audio.currentTime > 0.3")
            host_position=host.locator("#audio").evaluate("(audio) => audio.currentTime")
            guest_position=guest.locator("#audio").evaluate("(audio) => audio.currentTime")
            self.assertLess(abs(host_position-guest_position),1.0)
            self.assertEqual(errors,[])
            browser.close()

    def test_nas_ticket_confines_access_and_streams_ranges(self):
        _,_,session=self.request("/api/session",body={})
        token=json.loads(session)["token"]
        auth={"Authorization":"Bearer "+token}
        with sqlite3.connect(self.root/"data/pump.sqlite") as db:
            owner=db.execute("SELECT owner FROM sessions WHERE token_hash=?",(hashlib.sha256(token.encode()).hexdigest(),)).fetchone()[0]
        db.close()  # with закрывает только транзакцию: открытый дескриптор блокирует rmtree на Windows.
        media=(self.root/"music/alpha.mp3").read_bytes()
        received=[]
        class WebDavHandler(BaseHTTPRequestHandler):
            def do_GET(handler):
                received.append((handler.path,handler.headers.get("Range")))
                handler.send_response(206)
                handler.send_header("Content-Type","audio/mpeg")
                handler.send_header("Content-Length","100")
                handler.send_header("Content-Range",f"bytes 0-99/{len(media)}")
                handler.end_headers()
                handler.wfile.write(media[:100])
            def log_message(self,*args):
                pass
        nas=ThreadingHTTPServer(("127.0.0.1",0),WebDavHandler)
        thread=threading.Thread(target=nas.serve_forever,daemon=True)
        thread.start()
        self.addCleanup(nas.server_close)
        self.addCleanup(nas.shutdown)
        config=self.root/"nas.json"
        config.write_text(json.dumps([{"protocol":"webdav","id":"home","name":"Home NAS","base":f"http://127.0.0.1:{nas.server_port}/music/","owners":[owner]}]))
        self.stop()
        self.environment["PUMP_NAS_CONFIG"]=str(config)
        self.start()
        _,_,data=self.request("/api/nas",headers=auth)
        self.assertEqual(json.loads(data),[{"id":"home","name":"Home NAS"}])
        _,_,data=self.request("/api/nas/home/ticket",headers=auth,body={"path":"alpha.mp3"})
        ticket=json.loads(data)
        status,_,data=self.request(ticket["url"],headers={"Range":"bytes=0-99"})
        self.assertEqual((status,data),(206,media[:100]))
        self.assertEqual(received,[("/music/alpha.mp3","bytes=0-99")])
        _,_,data=self.request("/api/tracks/"+ticket["track"]["id"])
        self.assertEqual(json.loads(data)["source"],"NAS")
        _,_,another=self.request("/api/session",body={})
        _,_,data=self.request("/api/nas",headers={"Authorization":"Bearer "+json.loads(another)["token"]})
        self.assertEqual(json.loads(data),[])

    def test_provider_resolve_fallback_and_range_leases(self):
        media=(self.root/"music/alpha.mp3").read_bytes()
        received=[]
        class MediaHandler(BaseHTTPRequestHandler):
            def do_HEAD(handler):
                handler.serve(False)
            def do_GET(handler):
                handler.serve(True)
            def serve(handler,body):
                received.append((handler.path,handler.headers.get("Range"),handler.headers.get("Authorization"),handler.headers.get("Cookie")))
                if handler.path=="/failed.flac":
                    handler.send_error(503)
                    return
                start,end=0,len(media)-1
                if value:=handler.headers.get("Range"):
                    found=re.fullmatch(r"bytes=(\d+)-(\d*)",value)
                    if not found:
                        handler.send_error(416)
                        return
                    start=int(found[1]);end=min(int(found[2]) if found[2] else end,end)
                handler.send_response(206 if handler.headers.get("Range") else 200)
                handler.send_header("Content-Type","audio/mpeg")
                handler.send_header("Accept-Ranges","bytes")
                handler.send_header("Content-Length",str(end-start+1))
                if handler.headers.get("Range"):
                    handler.send_header("Content-Range",f"bytes {start}-{end}/{len(media)}")
                handler.end_headers()
                if body:
                    handler.wfile.write(media[start:end+1])
            def log_message(self,*args):
                pass
        upstream=ThreadingHTTPServer(("127.0.0.1",0),MediaHandler)
        threading.Thread(target=upstream.serve_forever,daemon=True).start()
        self.addCleanup(upstream.server_close)
        self.addCleanup(upstream.shutdown)
        base=f"http://127.0.0.1:{upstream.server_port}"
        plugins=self.root/"plugins";plugins.mkdir()
        (plugins/"test.wasm").write_bytes(provider_module(
            {"abi_version":1,"tracks":[{"id":"song-1","title":"Addon Song","artist":"WASM Test","kind":"preview","duration_ms":40000,"artwork":None}]},
            {"abi_version":1,"candidates":[{"url":base+"/failed.flac","quality":"lossless","mime_type":"audio/flac","bitrate_kbps":None},{"url":base+"/alpha.mp3","quality":"high","mime_type":"audio/mpeg","bitrate_kbps":96}]}
        ))
        self.stop()
        self.environment.update(PUMP_PLUGIN_DIR=str(plugins),PUMP_PLUGIN_ALLOWED_HOSTS="127.0.0.1")
        self.start()
        _,_,data=self.request("/api/providers/test/search?q=song")
        [track]=json.loads(data)["tracks"]
        self.assertEqual(track["provider"],{"provider_id":"test","track_id":"song-1"})
        request={"abi_version":1,"track_id":"song-1","quality":{"preferred":"high","prefer_lossless":True,"allow_fallback":True}}
        leases=[]
        for _ in range(2):
            _,_,data=self.request("/api/providers/test/resolve",body=request)
            leases.append(json.loads(data)["urls"])
        self.assertNotEqual(leases[0],leases[1])
        for urls in leases:
            with self.assertRaises(HTTPError) as failed:
                self.request(urls[0])
            self.assertEqual(failed.exception.code,502)
            status,headers,data=self.request(urls[1],headers={"Range":"bytes=100-149","Authorization":"Bearer ignored","Cookie":"test=private"})
            self.assertEqual((status,data),(206,media[100:150]))
            self.assertEqual(headers["Content-Range"],f"bytes 100-149/{len(media)}")
        self.assertTrue(all(auth is None and cookie is None for _,_,auth,cookie in received))
        with sync_playwright() as driver:
            browser=driver.chromium.launch(executable_path=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium"))
            page=browser.new_page(viewport={"width":390,"height":844})
            errors=[];page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            expect(page.locator('#source option[value="plugin:test"]')).to_have_count(1)
            page.locator("#source").select_option("plugin:test")
            page.locator("#qInput").fill("song")
            page.get_by_role("button",name="Addon Song",exact=False).click()
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > .3")
            self.assertTrue(page.locator("#audio").get_attribute("src").startswith("/addon/stream/"))
            self.assertEqual(page.evaluate("selected._providerIndex"),1)
            page.evaluate("()=>{audio.pause();audio.currentTime=3;selected._providerResolvedAt=0;requestPlay();}")
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > 3.1")
            self.assertEqual(errors,[])
            browser.close()

    def test_settings_profile_and_offline_audio(self):
        with sync_playwright() as driver:
            browser=driver.chromium.launch(executable_path=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium"))
            page=browser.new_page(**driver.devices["Pixel 7"])
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            self.assertTrue(page.evaluate("() => PumpUX.wasmReady"))
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button",name="North Loop",exact=False).click()
            wait_for_audio(page,"audio => audio.currentTime > 0.4")
            page.locator('[data-action="settings-open"]').click()
            self.assertTrue(page.evaluate("() => graph.ready"))
            page.locator("#concertMode").check()
            expect(page.locator("#pitchSemitones")).to_be_enabled()
            self.assertEqual(page.evaluate("async()=>{const context=graph.context;const original=context.setSinkId;let routed=null;context.setSinkId=async id=>{routed=id;};try{await setOutputDevice('test-speakers');return routed;}finally{context.setSinkId=original;}}"),"test-speakers")
            page.locator("#eqEnabled").check()
            page.locator("#autoEqFile").set_input_files({"name":"headphones.txt","mimeType":"text/plain","buffer":b"Preamp: -4 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1.4"})
            expect(page.locator("#settingsStatus")).to_have_text("Параметрический профиль применён.")
            page.get_by_text("Оформление",exact=True).click()
            page.locator("#designSelect").select_option("classic-glass")
            expect(page.locator("#artBox")).to_have_class(re.compile(r".*\bclassic-glass\b.*"))
            page.get_by_text("Хранилище",exact=True).click()
            page.locator("#downloadButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Запись доступна на этом устройстве.",timeout=15000)
            expect(page.locator("#downloadsList")).to_contain_text("North Loop")
            page.get_by_text("Библиотека",exact=True).click()
            page.locator("#importCsv").set_input_files({"name":"exportify.csv","mimeType":"text/csv","buffer":b"Track URI,Track Name,Artist Name(s),Duration (ms)\nspotify:track:test,North Loop,PUMP Test Artist,40000\nspotify:track:test,North Loop,PUMP Test Artist,40000\n"})
            expect(page.locator("#settingsStatus")).to_contain_text("Добавлено в очередь: 2",timeout=15000)
            if not page.locator('details[data-category^="Хранилище"]').evaluate("details=>details.open"):
                page.get_by_text("Хранилище",exact=True).click()
            page.locator("#downloadQueueButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Сохранено: 0. Уже на устройстве: 1. Ошибок: 0.")
            self.assertEqual(page.evaluate("async()=>{const tracks=(await api('/api/search?source=full&q=South')).tracks;queue.push(tracks[0]);return queue.length;}"),3)
            page.locator("#downloadQueueButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Сохранено: 1. Уже на устройстве: 1. Ошибок: 0.",timeout=15000)
            expect(page.locator("#downloadsList")).to_contain_text("South Loop")
            self.assertEqual(errors,[])
            browser.close()

    def test_local_analysis_and_two_deck_crossfade(self):
        _,_,data=self.request("/api/search?source=full&q=North")
        track=json.loads(data)["tracks"][0]
        _,_,session=self.request("/api/session",body={})
        auth={"Authorization":"Bearer "+json.loads(session)["token"]}
        with self.assertRaises(HTTPError) as unauthorized:
            self.request("/api/analysis/"+track["id"])
        self.assertEqual(unauthorized.exception.code,401)
        _,_,body=self.request("/api/analysis/"+track["id"],headers=auth,timeout=60)
        features=json.loads(body)
        self.assertGreater(features["duration_seconds"],39)
        self.assertGreater(features["true_peak"],0)
        self.assertIn("integrated_lufs",features)
        _,_,cached=self.request("/api/analysis/"+track["id"],headers=auth)
        self.assertEqual(json.loads(cached),features)
        with self.assertRaises(HTTPError) as external:
            self.request("/api/analysis/au-Public1",headers=auth)
        self.assertEqual(external.exception.code,404)
        with sync_playwright() as driver:
            executable=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser=driver.chromium.launch(executable_path=executable)
            page=browser.new_page(viewport={"width":390,"height":844})
            page.add_init_script("localStorage.setItem('pump.crossfadeSeconds','4')")
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("Loop")
            page.get_by_role("button",name="North Loop",exact=False).click()
            wait_for_audio(page,"audio => audio.currentTime > 0.6")
            page.evaluate("() => { window.firstDeck=audio; audio.currentTime=34; }")
            page.wait_for_function("() => transition?.phase==='fading'",timeout=10000)
            self.assertTrue(page.evaluate("() => !transition.from.paused && !transition.to.paused"))
            expect(page.locator("#tTitle")).to_have_text("South Loop",timeout=10000)
            self.assertTrue(page.evaluate("() => audio!==window.firstDeck && window.firstDeck.paused"))
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > 0.5 && audio.currentTime < 12")
            self.assertEqual(errors,[])
            browser.close()

    def test_lyrics_word_highlight_tick_and_pause(self):
        doc = {"format": "enhanced_lrc",
               "lines": [{"text": "one two", "start_ms": 1000, "end_ms": 3000,
                          "words": [{"text": "one", "start_ms": 1000, "end_ms": 2000, "singer_ids": [], "alignment": "auto", "role": None},
                                    {"text": "two", "start_ms": 2000, "end_ms": 3000, "singer_ids": [], "alignment": "auto", "role": None}],
                          "singer_ids": [], "alignment": "auto", "language": None, "role": None},
                         {"text": "three four", "start_ms": 3000, "end_ms": 5000,
                          "words": [{"text": "three", "start_ms": 3000, "end_ms": 4000, "singer_ids": [], "alignment": "auto", "role": None},
                                    {"text": "four", "start_ms": 4000, "end_ms": 5000, "singer_ids": [], "alignment": "auto", "role": None}],
                          "singer_ids": [], "alignment": "auto", "language": None, "role": None}],
               "singers": [], "metadata": {}, "instrumental": False}
        with sync_playwright() as driver:
            executable = os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser = driver.chromium.launch(executable_path=executable)
            page = browser.new_page(viewport={"width": 390, "height": 844})
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.route("**/api/lyrics/**", lambda route: route.fulfill(json={"synced": None, "plain": None, "document": doc}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button", name="North Loop", exact=False).click()
            wait_for_audio(page, "audio => audio.currentTime > 0.6")
            page.locator("#tabLyrics").click()
            page.locator("#audio").evaluate("(audio) => { audio.currentTime = 2.1; }")
            expect(page.locator(".lyric-word.current")).to_have_text("two", timeout=8000)
            page.locator("#audio").evaluate("(audio) => audio.pause()")
            page.wait_for_timeout(1200)
            self.assertEqual(page.locator(".lyric-word.current").inner_text(), "two")
            page.locator("#audio").evaluate("(audio) => { audio.currentTime = 3.5; return audio.play(); }")
            expect(page.locator(".lyric-word.current")).to_have_text("three", timeout=8000)
            # Seeking backward while paused must clear stale sung classes.
            page.locator("#audio").evaluate("(audio) => { audio.pause(); audio.currentTime = 1.5; }")
            expect(page.locator(".lyric-word.current")).to_have_text("one")
            self.assertEqual(page.locator(".lyric-word.sung").count(), 0)
            for _ in range(6):
                page.locator("#lyricEarlier").click()
            expect(page.locator("#lyricOffsetValue")).to_have_text("+600 мс")
            expect(page.locator(".lyric-word.current")).to_have_text("two")
            self.assertEqual(page.evaluate("() => JSON.parse(localStorage.getItem('pump.lyricOffsets'))[0][1]"),600)
            page.locator("#lyricReset").click()
            expect(page.locator(".lyric-word.current")).to_have_text("one")
            self.assertEqual(errors, [])
            browser.close()

    def test_lyrics_overlapping_vocals_and_gaps(self):
        doc={"format":"ttml","lines":[
            {"text":"lead","start_ms":1000,"end_ms":4000,
             "words":[{"text":"lead","start_ms":1000,"end_ms":4000}],"singer_ids":["lead"],"alignment":"left"},
            {"text":"harmony","start_ms":2000,"end_ms":3000,
             "words":[{"text":"harmony","start_ms":2000,"end_ms":3000}],"singer_ids":["harmony"],"alignment":"right"},
            {"text":"outro","start_ms":6000,"end_ms":8000,
             "words":[{"text":"outro","start_ms":6000,"end_ms":8000}],"singer_ids":["lead"],"alignment":"left"}]}
        with sync_playwright() as driver:
            executable=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser=driver.chromium.launch(executable_path=executable)
            page=browser.new_page(viewport={"width":390,"height":844})
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"plain":None,"document":doc}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button",name="North Loop",exact=False).click()
            wait_for_audio(page,"audio => audio.currentTime > 0.6")
            page.locator("#tabLyrics").click()
            page.locator("#audio").evaluate("(audio) => { audio.pause(); audio.currentTime=2.5; }")
            expect(page.locator(".l-line.active")).to_have_count(2)
            expect(page.locator(".lyric-word.current")).to_have_count(2)
            page.locator("#audio").evaluate("(audio) => { audio.currentTime=5; }")
            expect(page.locator(".l-line.active")).to_have_count(0)
            expect(page.locator(".lyric-word.current")).to_have_count(0)
            page.locator("#audio").evaluate("(audio) => { audio.currentTime=1.5; }")
            expect(page.locator(".lyric-word.current")).to_have_text("lead")
            expect(page.locator(".lyric-word.sung")).to_have_count(0)
            self.assertEqual(errors,[])
            browser.close()

    def test_two_people_listen_together(self):
        with sync_playwright() as driver:
            executable = os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser = driver.chromium.launch(executable_path=executable)
            device = driver.devices["Pixel 7"]
            host_context = browser.new_context(**device)
            guest_context = browser.new_context(**device)
            host, guest = host_context.new_page(), guest_context.new_page()
            errors = []
            for page in (host, guest):
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.route("**/api/lyrics/**", lambda route: route.fulfill(json={"synced": None, "plain": None}))
                page.goto(self.origin)
                self.assertTrue(page.locator("#audio").count())

            host.locator("#tabSearch").click()
            host.locator("#source").select_option("full")
            host.locator("#qInput").fill("North")
            host.get_by_role("button", name="North Loop", exact=False).click()
            expect(host.locator("#tTitle")).to_have_text("North Loop")
            wait_for_audio(host, "audio => audio.currentTime > 0.6")
            host.locator("#tabSearch").click()
            host.locator("#qInput").fill("South")
            expect(host.get_by_role("button", name="South Loop", exact=False)).to_be_visible()
            host.locator("#tabPlayer").click()
            expect(host.locator("#tTitle")).to_have_text("North Loop")
            host.locator("#queueButton").click()
            expect(host.locator("#queueList [aria-current]")).to_contain_text("North Loop")
            host.get_by_role("button", name="Закрыть очередь").click()

            host.locator("#tabRooms").click()
            host.locator("#createRoom").click()
            expect(host.locator("#roomStatus")).to_contain_text("Ведущий")
            code = host.locator("#roomCode").input_value()
            self.assertRegex(code, r"^[0-9a-f]{12}$")
            guest.locator("#tabRooms").click()
            guest.locator("#roomCode").fill(code)
            guest.get_by_role("button", name="Войти", exact=True).click()
            expect(guest.locator("#roomStatus")).to_contain_text("Слушатель")
            guest.locator("#enableRoomSound").click()
            guest.locator("#tabPlayer").click()
            expect(guest.locator("#playBtn")).to_be_disabled()
            expect(guest.locator("#tTitle")).to_have_text("North Loop")
            wait_for_audio(guest, "audio => !audio.paused")

            # Drop the live connection without leaving the room. The client
            # should reconnect with the same sound consent and follow the host.
            guest.evaluate("() => { window.previousRoomSocket = room.socket; room.socket.close(4000); }")
            guest.wait_for_function("() => room?.connected && room.socket !== window.previousRoomSocket", timeout=12000)
            wait_for_audio(guest, "audio => !audio.paused")
            self.assertTrue(guest.evaluate("() => room.sound"))
            host.locator("#tabPlayer").click()
            host.locator("#seekBar").evaluate(
                "(bar) => { bar.value = 50; bar.dispatchEvent(new Event('input', {bubbles: true})); }"
            )
            wait_for_audio(guest, "audio => audio.currentTime >= 19.8")
            host.locator("#playBtn").click()
            wait_for_audio(guest, "audio => audio.paused")
            host_position = host.locator("#audio").evaluate("(audio) => audio.currentTime")
            guest_position = guest.locator("#audio").evaluate("(audio) => audio.currentTime")
            self.assertLess(abs(host_position - guest_position), 0.2)

            # Resume announces a future server-clock anchor. Both peers
            # prebuffer instead of immediately starting at message arrival.
            guest.evaluate("""() => {
                window.scheduledStartSeen = null;
                room.socket.addEventListener("message", event => {
                    const msg=JSON.parse(event.data);
                    if(msg.type==="playback"&&msg.playback.playing&&msg.playback.at_ms>roomNow()+room.skew+100){
                        window.scheduledStartSeen={paused:audio.paused,at:msg.playback.at_ms};
                    }
                });
            }""")
            host.locator("#playBtn").click()
            guest.wait_for_function("() => window.scheduledStartSeen !== null",timeout=8000)
            self.assertTrue(guest.evaluate("() => window.scheduledStartSeen.paused"))
            wait_for_audio(host,"audio => !audio.paused")
            wait_for_audio(guest,"audio => !audio.paused")
            # Compare projected media positions rather than sequential reads.
            # This is browser timeline evidence, not speaker-output latency.
            time.sleep(0.7)
            sample="audio => ({position:audio.currentTime,at:performance.timeOrigin+performance.now(),rate:audio.playbackRate})"
            h=host.locator("#audio").evaluate(sample)
            g=guest.locator("#audio").evaluate(sample)
            projected=h["position"]+(g["at"]-h["at"])*h["rate"]/1000
            self.assertLess(abs(projected-g["position"]),0.4)

            host.locator("#tabStats").click()
            expect(host.locator("#statsTracks")).to_contain_text("North Loop")
            host.locator("#statsPeriod").select_option("month")
            expect(host.locator("#statsArtists")).to_contain_text("PUMP Test Artist")
            self.assertEqual(errors, [])
            browser.close()

    def test_soundcloud_playback_range_seek_and_refresh(self):
        tracks = self.soundcloud_live_tracks("lofi")
        track = tracks[0]
        self.assertTrue(track["audio_url"].startswith("/source/sc/"))
        status, headers, data = self.request(track["audio_url"])
        self.assertEqual(status, 200)
        self.assertEqual(headers.get_content_type(), "audio/mpeg")
        self.assertGreater(len(data), 1024)
        status, headers, part = self.request(track["audio_url"], headers={"Range": "bytes=0-99"})
        self.assertEqual(status, 206)
        self.assertEqual(part, data[:100])
        self.assertTrue(headers["Content-Range"].startswith("bytes 0-99/"))
        # Подпись свежая на каждый запрос: повторное resolve тоже 206 с тем же срезом.
        status, _, again = self.request(track["audio_url"], headers={"Range": "bytes=100-199"})
        self.assertEqual(status, 206)
        self.assertEqual(again, data[100:200])
        # Метаданные доступны комнатам и статистике без повторного поиска.
        _, _, meta = self.request("/api/tracks/" + track["id"])
        self.assertEqual(json.loads(meta)["id"], track["id"])

    def test_room_follows_soundcloud_track(self):
        tracks = self.soundcloud_live_tracks("lofi")
        assert tracks, "see soundcloud_live_tracks"
        with sync_playwright() as driver:
            executable = os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser = driver.chromium.launch(executable_path=executable)
            device = driver.devices["Pixel 7"]
            host, guest = browser.new_context(**device).new_page(), browser.new_context(**device).new_page()
            errors = []
            for page in (host, guest):
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.route("**/api/lyrics/**", lambda route: route.fulfill(json={"synced": None, "plain": None}))
                page.goto(self.origin)
            host.locator("#tabSearch").click()
            host.locator("#source").select_option("soundcloud")
            host.locator("#qInput").fill("lofi")
            host.locator("#list .item").first.wait_for()
            title = host.locator("#list .item .item-t").first.inner_text()
            host.locator("#list .item").first.click()
            expect(host.locator("#tTitle")).to_have_text(title)
            wait_for_audio(host, "audio => !audio.paused && audio.currentTime > 0.5")
            host.locator("#tabRooms").click()
            host.locator("#createRoom").click()
            expect(host.locator("#roomStatus")).to_contain_text("Ведущий")
            code = host.locator("#roomCode").input_value()
            guest.locator("#tabRooms").click()
            guest.locator("#roomCode").fill(code)
            guest.get_by_role("button", name="Войти", exact=True).click()
            expect(guest.locator("#roomStatus")).to_contain_text("Слушатель")
            guest.locator("#enableRoomSound").click()
            expect(guest.locator("#tTitle")).to_have_text(title)
            wait_for_audio(guest, "audio => !audio.paused && audio.currentTime > 0.5")
            self.assertEqual(errors, [])
            browser.close()

    def test_lyrics_sync_seek_rate_pause(self):
        lrc = ("[00:01.00] One" + chr(10) + "[00:03.00] Two" + chr(10)
               + "[00:05.00] Three" + chr(10) + "[00:07.00] Four" + chr(10))
        with sync_playwright() as driver:
            executable = os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser = driver.chromium.launch(executable_path=executable)
            page = browser.new_page(viewport={"width": 390, "height": 844})
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.route("**/api/lyrics/**", lambda route: route.fulfill(json={"synced": lrc, "plain": None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button", name="North Loop", exact=False).click()
            wait_for_audio(page, "audio => audio.currentTime > 0.6")
            page.locator("#tabLyrics").click()
            # Перемотка ставит активную строку по времени.
            page.locator("#audio").evaluate("(audio) => { audio.currentTime = 4.2; }")
            expect(page.locator(".l-line.active")).to_have_text("Two", timeout=8000)
            # Медленная скорость видна: за 1.7 с строка не успевает смениться
            # (на 1.0x "Four" появилась бы через 1.0 c, на 0.5x нужно 2.0 c).
            page.locator("#audio").evaluate("(audio) => { audio.currentTime = 6.0; audio.playbackRate = 0.5; }")
            page.wait_for_timeout(1700)
            self.assertEqual(page.locator(".l-line.active").inner_text(), "Three")
            expect(page.locator(".l-line.active")).to_have_text("Four", timeout=8000)
            # Пауза в середине строки фиксирует её; продолжение — без скачка.
            page.locator("#audio").evaluate("(audio) => { audio.currentTime = 4.2; }")
            expect(page.locator(".l-line.active")).to_have_text("Two", timeout=8000)
            page.locator("#audio").evaluate("(audio) => audio.pause()")
            page.wait_for_timeout(1500)
            self.assertEqual(page.locator(".l-line.active").inner_text(), "Two")
            page.locator("#audio").evaluate("(audio) => audio.play()")
            wait_for_audio(page, "audio => !audio.paused")
            self.assertEqual(page.locator(".l-line.active").inner_text(), "Two")
            self.assertEqual(errors, [])
            browser.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
