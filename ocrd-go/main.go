// ocrd-go — EXPERIMENT: Go port of the Rust ocrd daemon, built to answer one
// question: what does the Windows build cost when the pipeline is the
// benchmark's GOFAST (custom PNG decoder + amd64 asm) and the tesseract
// binding is a thin cgo wrapper over capi.h?
//
// IT IS KEPT IN STEP WITH THE RUST DAEMON ON PURPOSE. It is never published
// (see the Makefile), so its only value is as the cheapest available check
// that this protocol is implementable from its written description rather than
// only from the Rust source — and a second implementation that has drifted off
// the current protocol proves nothing at all.
//
// Protocol is byte-compatible with the Rust daemon: NATS request/reply on
// <prefix>.ocr.read (queue group "ocrd") and <prefix>.ocr.version. See
// ../ocrd-rust/src/main.rs for the full rationale; the short version is that a
// loopback TCP port plus a filesystem path assumed the daemon and its caller
// share a machine, and on the reference workstation (Linux daemon in a WSL2
// container, Windows caller) they do not.
package main

import (
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"image"
	"log"
	"ocrdgo/pipeline"
	"os"
	"runtime"

	"github.com/nats-io/nats.go"
)

// ocrRequest is one page to read. The image travels INLINE as base64 — there
// is no path field and no path fallback, because a path is exactly the
// assumption this transport removes. Every other field is an OCR knob the
// caller owns; the daemon is generic and knows nothing about the caller's
// domain.
type ocrRequest struct {
	Image       string `json:"image"`
	PSM         int    `json:"psm"`
	Lang        string `json:"lang"`
	DPI         int    `json:"dpi"`
	Upscale     int    `json:"upscale"`
	PixelBudget int64  `json:"pixel_budget"`
}

// ocrResponse always carries both fields, even when empty: a decoder that must
// tell "absent" from "empty" has one more state to get wrong for no gain. A
// non-empty Error means that read failed and THE REPLY IS STILL SENT — a
// silently dropped request is indistinguishable from a pass at the caller.
type ocrResponse struct {
	Text  string `json:"text"`
	Error string `json:"error"`
}

// versionResponse is the identity proof. Answering on a per-run-unique subject
// is what a TCP port could never do: it proves WHICH daemon replied, not merely
// that something is listening.
type versionResponse struct {
	Version string `json:"version"`
	Impl    string `json:"impl"`
}

// scaleFor mirrors the Rust daemon's budget clamp: requested factor, halved
// until the scaled plane fits the pixel budget.
func scaleFor(w, h, want int, budget int64) int {
	s := want
	for s > 1 && int64(w)*int64(h)*int64(s)*int64(s) > budget {
		s--
	}
	if s < 1 {
		s = 1
	}
	return s
}

var tessData, tessLang string

// handleRead turns one request payload into one reply payload. Every failure
// path lands in an Error reply rather than a dropped message, because the
// caller is blocked on a reply only this function can produce.
func handleRead(eng *tessEngine, payload []byte) ocrResponse {
	fail := func(format string, a ...any) ocrResponse {
		return ocrResponse{Error: fmt.Sprintf(format, a...)}
	}
	var req ocrRequest
	if err := json.Unmarshal(payload, &req); err != nil {
		return fail("malformed request: %v", err)
	}
	raw, err := base64.StdEncoding.DecodeString(req.Image)
	if err != nil {
		return fail("decode base64 image: %v", err)
	}
	if len(raw) == 0 {
		return fail("empty image")
	}

	// Absent OR zero means default: a caller serialising from a struct sends
	// the zero value rather than omitting the field, and a psm of 0 is an unset
	// field, not a request for segmentation mode 0.
	upscale, budget := req.Upscale, req.PixelBudget
	if upscale <= 0 {
		upscale = 1
	}
	if budget <= 0 {
		budget = 20_000_000
	}
	psm, dpi := req.PSM, req.DPI
	if psm <= 0 {
		psm = 3
	}
	if dpi <= 0 {
		dpi = 300
	}

	// Plan the scale from the PNG header alone (IHDR at fixed offset 16): when
	// it comes out 1, the frame is never decoded here at all. A payload with no
	// readable IHDR — a JPEG, which the contract also admits — plans as scale 1
	// and goes to leptonica, which sniffs the format itself. Refusing it here
	// would turn a supported format into a read failure.
	scale := 1
	if len(raw) >= 24 && string(raw[1:4]) == "PNG" {
		w0 := int(binary.BigEndian.Uint32(raw[16:20]))
		h0 := int(binary.BigEndian.Uint32(raw[20:24]))
		scale = scaleFor(w0, h0, upscale, budget)
	}

	var text string
	if scale <= 1 {
		// No upscaling would happen: skip preprocessing entirely and let
		// tesseract's fast RGB path binarize the original image. The gray
		// plane is pathological on ultra-wide pages: 177s vs 3.2s measured
		// on a 19599x1002 screenshot (see tess.go recognizeMem).
		text, err = eng.recognizeMem(raw, psm, dpi)
	} else {
		decoded, derr := pipeline.Decode(raw)
		if derr != nil {
			return fail("decode: %v", derr)
		}
		g := pipeline.Preprocess(decoded, scale)
		text, err = eng.recognize(g, psm, dpi)
	}
	if err != nil {
		return fail("%v", err)
	}
	return ocrResponse{Text: text}
}

// versionNumber tracks ocrd-rust/Cargo.toml's `version`, and is the ONLY place
// in this tree that spells it.
//
// Go has no compile-time equivalent of Rust's CARGO_PKG_VERSION for a main
// package — debug.ReadBuildInfo reports "(devel)" for the main module on an
// ordinary build — so this cannot be derived the way the Rust daemon derives
// it, and a human has to move it. It is still worth stating once: the two
// implementations are meant to be indistinguishable, so a Go daemon reporting a
// stale version while the Rust one is correct is a trap for whoever debugs the
// pair next.
const versionNumber = "0.2.0"

// version is the daemon build identity, reported by --version and on the
// .ocr.version subject.
const version = "ocrd-go " + versionNumber

// readQueueGroup is fixed, not configurable: the group name is the mechanism
// that makes N subscribers share one request stream instead of each receiving a
// copy, and a caller that could set it could only ever set it wrong.
const readQueueGroup = "ocrd"

func main() {
	// ONE SPELLING PER FLAG, SHARED WITH THE OTHER IMPLEMENTATION. A client
	// spawns "the daemon" and cannot know which implementation it started, so
	// the argv must be identical across both. Go's flag package accepts
	// --nats and -nats interchangeably; the Rust build accepts only the
	// double-dash form, so the double-dash spelling is the only one documented.
	natsURL := flag.String("nats", "", "NATS server URL, e.g. nats://127.0.0.1:41234")
	subjectPrefix := flag.String("subject-prefix", "", "per-run-unique subject prefix")
	workers := flag.Int("workers", 0, "concurrent readers (default: NumCPU)")
	showVersion := flag.Bool("version", false, "print version and exit")
	flag.StringVar(&tessData, "tessdata", os.Getenv("TESSDATA_PREFIX"), "tessdata directory")
	flag.StringVar(&tessLang, "lang", "eng", "tesseract language")
	flag.Parse()

	// --version prints and exits before any connection or model load, so an
	// image build can prove the binary it just downloaded executes on that
	// userland with no tessdata present and no NATS server running.
	if *showVersion {
		fmt.Println(version)
		return
	}

	// REQUIRED, with no defaults on purpose. A default URL invites the daemon
	// to attach to whatever server happens to be listening, and a default
	// prefix throws away the identity guarantee the per-run-unique prefix
	// exists to provide. Both would fail as a silently wrong answer instead of
	// a loud refusal.
	if *natsURL == "" || *subjectPrefix == "" {
		log.Fatalf("usage: ocrd --nats <url> --subject-prefix <prefix> [--workers <n>]")
	}
	if *workers <= 0 {
		*workers = runtime.NumCPU()
	}

	// Only Windows gets a hardcoded fallback, and only because vcpkg installs
	// no tessdata and sets no environment. On Linux an EMPTY datapath is the
	// correct answer, not a guess: tesseract resolves its own compiled-in
	// prefix (the distro package puts the traineddata there), so guessing a
	// path here could only ever be wrong. A Windows path baked into a Linux
	// build fails engine init outright.
	if tessData == "" && runtime.GOOS == "windows" {
		tessData = "C:/Program Files/Tesseract-OCR/tessdata"
	}

	// FAIL LOUD, FAIL NOW. There is no fallback transport and no degraded mode:
	// a daemon that cannot reach its server can never answer a request, and the
	// one thing worse than not starting is appearing to have started. RetryOnFailedConnect
	// stays OFF for the same reason — the caller is waiting on a version reply
	// that would never come.
	nc, err := nats.Connect(*natsURL, nats.Name("ocrd-go"))
	if err != nil {
		log.Fatalf("FATAL: cannot connect to NATS at %s: %v; no fallback transport exists", *natsURL, err)
	}
	log.Printf("connected to %s (server max_payload %d bytes)", *natsURL, nc.MaxPayload())

	readSubject := *subjectPrefix + ".ocr.read"
	versionSubject := *subjectPrefix + ".ocr.version"

	// THE IDENTITY SUBJECT, and the reason a plain Subscribe is right here: the
	// caller asks a prefix only this run knows, so exactly one daemon can
	// possibly answer. There is no group to share and nothing to balance.
	verBody, err := json.Marshal(versionResponse{Version: version, Impl: "go"})
	if err != nil {
		log.Fatalf("FATAL: encode version reply: %v", err)
	}
	if _, err := nc.Subscribe(versionSubject, func(m *nats.Msg) {
		if err := m.Respond(verBody); err != nil {
			log.Printf("respond version: %v", err)
		}
	}); err != nil {
		log.Fatalf("FATAL: subscribe %s: %v", versionSubject, err)
	}

	// EXACTLY N SUBSCRIBERS, EXACTLY N CONCURRENT READS. Each goroutine owns
	// its own subscription to the queue group and its own resident engine, and
	// handles one message at a time to completion. The server hands each
	// request to one group member, so the in-flight count cannot exceed the
	// member count — that IS the bound. There is deliberately no semaphore and
	// no worker-slot channel here (the TCP version had one): that is a second
	// scheduler on top of the queue group's own, with two places to get the
	// bound wrong, and it parks requests inside this process where the server
	// can no longer redeliver them to another daemon.
	for i := 0; i < *workers; i++ {
		sub, err := nc.QueueSubscribeSync(readSubject, readQueueGroup)
		if err != nil {
			// A worker that cannot subscribe silently shrinks the pool, which
			// shows up later as unexplained slowness. Refuse to run in a shape
			// nobody asked for.
			log.Fatalf("FATAL: worker %d subscribe %s: %v", i, readSubject, err)
		}
		// UNLIMITED PENDING. nats.go defaults a subscription to 64 MB of
		// pending bytes and drops messages past it as a slow consumer — with
		// page images up to the server's 32 MB limit that is two messages, and
		// a dropped request is a caller blocked forever on a reply that will
		// never be sent. Queued requests are cheap to hold; losing one is not.
		if err := sub.SetPendingLimits(-1, -1); err != nil {
			log.Fatalf("FATAL: worker %d pending limits: %v", i, err)
		}
		go func(id int, sub *nats.Subscription) {
			// The engine is created LAZILY, on the first message, so a worker
			// that never receives one never pays for a model load.
			var eng *tessEngine
			for {
				// No deadline: the daemon waits for work for as long as it
				// runs, and a timeout here would only mean re-entering the
				// same wait one loop later.
				msg, err := sub.NextMsgWithContext(context.Background())
				if err != nil {
					log.Fatalf("FATAL: worker %d receive: %v", id, err)
				}
				if eng == nil {
					if eng, err = newTessEngine(tessData, tessLang); err != nil {
						// The engine is per-worker and permanent; failing to
						// build it is not a per-request condition.
						log.Fatalf("FATAL: worker %d tesseract init: %v", id, err)
					}
				}
				resp := handleRead(eng, msg.Data)
				body, err := json.Marshal(resp)
				if err != nil {
					body = []byte(`{"text":"","error":"encode reply failed"}`)
				}
				if msg.Reply == "" {
					log.Printf("request on %s carried no reply subject; dropping", readSubject)
					continue
				}
				if err := msg.Respond(body); err != nil {
					// The answer is lost and the caller is still waiting. Loud,
					// but not fatal: killing the daemon would strand every
					// other in-flight read too.
					log.Printf("respond: %v", err)
				}
			}
		}(i, sub)
	}

	log.Printf("%d workers on %s (queue group %s); identity on %s",
		*workers, readSubject, readQueueGroup, versionSubject)

	// Park forever. The daemon's warm engines are the asset; it runs until the
	// machine or an operator stops it.
	select {}
}

// referenced by fastpng.go's factor selection; kept for parity with the bench.
func ocrUpscaleFactorFor(b image.Rectangle) int {
	px := b.Dx() * b.Dy()
	if px <= 0 {
		return 1
	}
	s := 3
	for s > 1 && int64(px)*int64(s)*int64(s) > 20_000_000 {
		s--
	}
	return s
}
