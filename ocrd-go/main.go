// ocrd-go — EXPERIMENT: Go port of the Rust ocrd daemon, built to answer one
// question: what does the Windows build cost when the pipeline is the
// benchmark's GOFAST (custom PNG decoder + amd64 asm) and the tesseract
// binding is a thin cgo wrapper over capi.h?
//
// Protocol is byte-compatible with the Rust daemon: NDJSON over loopback TCP,
// request {id,path,psm,lang,dpi,upscale,pixel_budget}, response
// {id,text,error}, responses may arrive out of order.
package main

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"image"
	"log"
	"net"
	"ocrdgo/pipeline"
	"os"
	"runtime"
	"sync"
)

type ocrRequest struct {
	ID          string `json:"id"`
	Path        string `json:"path"`
	PSM         int    `json:"psm"`
	Lang        string `json:"lang"`
	DPI         int    `json:"dpi"`
	Upscale     int    `json:"upscale"`
	PixelBudget int64  `json:"pixel_budget"`
}

type ocrResponse struct {
	ID    string `json:"id"`
	Text  string `json:"text"`
	Error string `json:"error"`
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

var enginePool sync.Pool
var tessData, tessLang string

func getEngine() (*tessEngine, error) {
	if e, ok := enginePool.Get().(*tessEngine); ok && e != nil {
		return e, nil
	}
	return newTessEngine(tessData, tessLang)
}

func handleRequest(req ocrRequest, out chan<- ocrResponse) {
	fail := func(err error) { out <- ocrResponse{ID: req.ID, Error: err.Error()} }
	raw, err := os.ReadFile(req.Path)
	if err != nil {
		fail(err)
		return
	}
	upscale, budget := req.Upscale, req.PixelBudget
	if upscale <= 0 {
		upscale = 1
	}
	if budget <= 0 {
		budget = 20_000_000
	}
	// Plan the scale from the PNG header alone (IHDR at fixed offset 16):
	// when it comes out 1, the frame is never decoded here at all.
	if len(raw) < 24 {
		fail(fmt.Errorf("truncated PNG %s", req.Path))
		return
	}
	w0 := int(binary.BigEndian.Uint32(raw[16:20]))
	h0 := int(binary.BigEndian.Uint32(raw[20:24]))
	scale := scaleFor(w0, h0, upscale, budget)
	psm, dpi := req.PSM, req.DPI
	if psm <= 0 {
		psm = 3
	}
	if dpi <= 0 {
		dpi = 300
	}
	eng, err := getEngine()
	if err != nil {
		fail(err)
		return
	}
	var text string
	if scale <= 1 {
		// No upscaling would happen: skip preprocessing entirely and let
		// tesseract's fast RGB path binarize the original file. The gray
		// plane is pathological on ultra-wide pages: 177s vs 3.2s measured
		// on a 19599x1002 screenshot (see tess.go recognizeFile).
		text, err = eng.recognizeFile(req.Path, psm, dpi)
	} else {
		decoded, derr := pipeline.Decode(raw)
		if derr != nil {
			enginePool.Put(eng)
			fail(fmt.Errorf("decode %s: %w", req.Path, derr))
			return
		}
		g := pipeline.Preprocess(decoded, scale)
		text, err = eng.recognize(g, psm, dpi)
	}
	enginePool.Put(eng)
	if err != nil {
		fail(err)
		return
	}
	out <- ocrResponse{ID: req.ID, Text: text}
}

func serveConn(conn net.Conn, slots chan struct{}) {
	defer conn.Close()
	out := make(chan ocrResponse, 64)
	done := make(chan struct{})
	go func() {
		enc := json.NewEncoder(conn)
		for r := range out {
			if err := enc.Encode(r); err != nil {
				break
			}
		}
		close(done)
	}()
	var wg sync.WaitGroup
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 1<<20), 1<<20)
	for sc.Scan() {
		var req ocrRequest
		if err := json.Unmarshal(sc.Bytes(), &req); err != nil {
			continue
		}
		wg.Add(1)
		slots <- struct{}{}
		go func(r ocrRequest) {
			defer wg.Done()
			defer func() { <-slots }()
			handleRequest(r, out)
		}(req)
	}
	wg.Wait()
	close(out)
	<-done
}

func main() {
	addr := flag.String("addr", "127.0.0.1:40066", "listen address")
	flag.StringVar(&tessData, "tessdata", os.Getenv("TESSDATA_PREFIX"), "tessdata directory")
	flag.StringVar(&tessLang, "lang", "eng", "tesseract language")
	flag.Parse()
	if tessData == "" {
		tessData = "C:/Program Files/Tesseract-OCR/tessdata"
	}
	// Fail fast if the engine cannot come up at all.
	probe, err := newTessEngine(tessData, tessLang)
	if err != nil {
		log.Fatalf("tesseract init: %v", err)
	}
	enginePool.Put(probe)
	ln, err := net.Listen("tcp", *addr)
	if err != nil {
		log.Fatalf("bind %s: %v", *addr, err)
	}
	log.Printf("ocrd-go listening on %s (workers<=%d)", *addr, runtime.NumCPU())
	slots := make(chan struct{}, runtime.NumCPU())
	for {
		conn, err := ln.Accept()
		if err != nil {
			log.Fatalf("accept: %v", err)
		}
		go serveConn(conn, slots)
	}
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
