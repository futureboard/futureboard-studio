// Futureboard Visualizer: a virtual-LCD meter and analysis display for
// Futureboard Studio. Reads Studio's shared-memory visualizer link
// (crates/VisualizerLink) — transport, every track's levels, the master's
// post-fader audio and one requested track's audio — and draws it on the
// lcdemu virtual LCD.

#include <SDL3/SDL.h>
#include <SDL3/SDL_main.h>

#include "Analysis.hpp"
#include "Canvas.hpp"
#include "LcdRenderer.hpp"
#include "MeterViews.hpp"
#include "link/FutureboardLink.hpp"
#include "mixviz/Views.hpp"
#ifdef _WIN32
#include "mixviz/DesktopBar.hpp"
#endif

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

using namespace fbvis;
using mixviz::Rect;

namespace {

constexpr int kMinW = 256, kH = 128, kHeaderH = 9;
constexpr uint8_t kMid = 150;

enum class Tab { Meters, Mixer, Loudness, Spectrum, Stereo, Scope, Overview, Track, Transport, Count };
constexpr const char* kTabNames[] = {"METERS", "MIXER", "LOUDNESS", "SPECTRUM", "STEREO",
                                     "SCOPE",  "OVERVIEW", "TRACK", "TRANSPORT"};
constexpr int kTabCount = static_cast<int>(Tab::Count);

struct Toast {
    std::string text;
    float timeLeft = 0.0f;
    void show(std::string t, float secs = 1.6f) {
        text = std::move(t);
        timeLeft = secs;
    }
    void draw(lcd::Canvas& c) const {
        if (timeLeft <= 0.0f) return;
        const std::string s = lcdText(text, 40);
        const int w = lcd::Canvas::textWidth(s) + 14;
        const int x = (c.width() - w) / 2, y = 52;
        c.fillRect(x, y, w, 21, 0);
        c.rect(x, y, w, 21);
        c.rect(x + 2, y + 2, w - 4, 17);
        c.textCentered(c.width() / 2, y + 7, s);
    }
};

void drawHeader(lcd::Canvas& c, int x, int w, const std::string& left, const std::string& right) {
    c.fillRect(x, 0, w, kHeaderH, 255);
    c.text(x + 2, 1, left, 1, 0);
    c.text(x + w - 2 - lcd::Canvas::textWidth(right), 1, right, 1, 0);
}

void drawQuadTitle(lcd::Canvas& c, int x, int y, const std::string& t) {
    const int w = lcd::Canvas::textWidth(t) + 3;
    c.fillRect(x, y, w, 8, 255);
    c.text(x + 1, y, t, 1, 0);
}

// A canvas this wide (the docked bar) shows the four views side by side,
// full size, each under its own header; a narrower one in quarters.
constexpr int kWideCanvas = 4 * kMinW - 64;

void drawWideQuad(lcd::Canvas& c, int w, const mixviz::FrameData& fd, mixviz::Imager& imager,
                  const std::string& tag, const std::string& status) {
    const int pw = w / 4;
    const Rect body{0, kHeaderH + 1, pw, kH - kHeaderH - 1};
    const auto at = [&](int i) { return Rect{i * pw, body.y, i == 3 ? w - 3 * pw : pw - 1, body.h}; };
    drawHeader(c, 0, pw - 1, tag + " LOUDNESS", "");
    drawLoudness(c, at(0), fd, false);
    drawHeader(c, pw, pw - 1, "SPECTRUM", "");
    drawSpectrum(c, at(1), fd, false);
    drawHeader(c, 2 * pw, pw - 1, "IMAGER", "");
    drawImager(c, at(2), imager, fd, false);
    drawHeader(c, 3 * pw, w - 3 * pw, "SCOPE", status);
    drawScope(c, at(3), fd, false);
    for (int i = 1; i < 4; ++i)
        for (int y = kHeaderH; y < kH; y += 2) c.pixel(i * pw - 1, y, 120);
}

// Loudness, spectrum, imager and scope in four quarters.
void drawQuad(lcd::Canvas& c, int w, const mixviz::FrameData& fd, mixviz::Imager& imager, const std::string& tag,
              const std::string& status) {
    if (w >= kWideCanvas) {
        drawWideQuad(c, w, fd, imager, tag, status);
        return;
    }
    const int top = 0, hh = kH / 2, hw = w / 2;
    drawLoudness(c, {0, top, hw - 1, hh - 1}, fd, true);
    drawSpectrum(c, {hw, top, w - hw, hh - 1}, fd, true);
    drawImager(c, {0, top + hh, hw - 1, kH - top - hh}, imager, fd, true);
    drawScope(c, {hw, top + hh, w - hw, kH - top - hh}, fd, true);
    for (int i = top; i < kH; i += 2) c.pixel(hw - 1, i, 120);
    for (int i = 0; i < w; i += 2) c.pixel(i, top + hh - 1, 120);
    drawQuadTitle(c, 0, top, tag + " LUFS");
    drawQuadTitle(c, hw, top, "SPECTRUM");
    drawQuadTitle(c, 0, top + hh, "IMAGER");
    drawQuadTitle(c, hw, top + hh, "SCOPE");
}

void drawCentered(lcd::Canvas& c, int w, const std::string& a, const std::string& b) {
    c.textCentered(w / 2, 52, lcdText(a, 60), 1);
    if (!b.empty()) c.textCentered(w / 2, 66, lcdText(b, 60), 1, kMid);
}

void drawHelp(lcd::Canvas& c, int w) {
    static const char* lines[] = {
        "1-9 TAB   TAB/SHIFT CYCLE  F1 HELP",
        "UP/DN TRACK  F FOLLOW STUDIO SELECTION",
        "LEFT/RIGHT OR WHEEL  PAGE",
        "CLICK METER  WATCH THAT TRACK",
        "L LIST/BRIDGE  R RESET LOUDNESS",
        "S TILT  A IMAGER GAIN  [ ] SCOPE",
        "T THEME  G GRID  H GHOST  +/- LIGHT",
        "SPACE HOLD  P SAVE BMP  F11 FULL",
        "B DOCK TO / UNDOCK FROM TOP OF DESKTOP",
        "CTRL+UP/DN  BAR HEIGHT 128-320 PX",
    };
    constexpr int kLines = static_cast<int>(sizeof lines / sizeof lines[0]);
    const int bw = 240, bh = kLines * 9 + 7, x = (w - bw) / 2, y = (kH - bh) / 2 + 4;
    c.fillRect(x, y, bw, bh, 0);
    c.rect(x, y, bw, bh);
    for (int i = 0; i < kLines; ++i) c.text(x + 6, y + 4 + i * 9, lines[i], 1, i == 0 ? 255 : 200);
}

std::string statusText(const Session& s, bool hold) {
    if (!s.connected) return "NO STUDIO";
    const auto& f = s.frame;
    char b[48];
    const char* state = (f.transport & fblink::transport::Recording) ? "REC"
                        : (f.transport & fblink::transport::Playing) ? "PLAY"
                                                                     : "STOP";
    std::snprintf(b, sizeof b, "%s%u/%u %.1f %s", hold ? "HOLD " : "", f.time_sig_num, f.time_sig_den,
                  static_cast<double>(f.bpm), state);
    return b;
}

struct Options {
    Tab tab = Tab::Meters;
    std::string linkName = fblink::kRegionName;
    std::string snapPath;
    float snapAfter = 3.0f;
    // Docked along the top of the desktop (Windows), like lcdemu's mixviz;
    // --window opens a normal window instead.
    bool bar = true;
    int barHeight = 150;
    int width = 1280, height = 640;
    int theme = 0;
    bool grid = false;
    bool list = false;
    int watch = 0; // 1-based track to watch at start; 0 follows Studio
};

// Docked heights Ctrl+Up / Ctrl+Down step through. 128 is one screen pixel
// per LCD row: the smallest strip that still shows every row.
constexpr int kBarHeights[] = {128, 150, 200, 240, 320};

int stepBarHeight(int current, int delta) {
    constexpr int n = static_cast<int>(sizeof kBarHeights / sizeof kBarHeights[0]);
    int index = 0;
    while (index < n - 1 && kBarHeights[index] < current) ++index;
    if (delta < 0 && kBarHeights[index] >= current) --index; // between presets: go to the one below
    else if (delta > 0 && kBarHeights[index] == current) ++index;
    return kBarHeights[std::clamp(index, 0, n - 1)];
}

// The bar height, tab and theme last used, kept in the user's app-data
// folder. Command-line options override them.
std::string settingsPath() {
    char* dir = SDL_GetPrefPath("Futureboard", "Visualizer");
    if (!dir) return {};
    std::string path = std::string(dir) + "settings.txt";
    SDL_free(dir);
    return path;
}

void loadSettings(Options& o) {
    const std::string path = settingsPath();
    std::ifstream in(path);
    std::string line;
    while (std::getline(in, line)) {
        const size_t eq = line.find('=');
        if (eq == std::string::npos) continue;
        const std::string key = line.substr(0, eq);
        const int value = std::atoi(line.c_str() + eq + 1);
        if (key == "height") o.barHeight = std::clamp(value, 128, 2000);
        else if (key == "tab") o.tab = static_cast<Tab>(std::clamp(value - 1, 0, kTabCount - 1));
        else if (key == "theme") o.theme = std::max(0, value - 1);
    }
}

void saveSettings(int barHeight, Tab tab, size_t theme) {
    const std::string path = settingsPath();
    if (path.empty()) return;
    std::ofstream out(path, std::ios::trunc);
    out << "height=" << barHeight << "\ntab=" << static_cast<int>(tab) + 1 << "\ntheme=" << theme + 1 << "\n";
}

Options parse(int argc, char** argv, Options o) {
    for (int i = 1; i < argc; ++i) {
        const auto is = [&](const char* flag) { return !std::strcmp(argv[i], flag); };
        const bool hasValue = i + 1 < argc;
        if (is("--tab") && hasValue) {
            o.tab = static_cast<Tab>(std::clamp(std::atoi(argv[++i]) - 1, 0, kTabCount - 1));
        } else if (is("--link") && hasValue) {
            o.linkName = argv[++i];
        } else if (is("--snap") && hasValue) {
            o.snapPath = argv[++i];
        } else if (is("--snap-after") && hasValue) {
            o.snapAfter = static_cast<float>(std::atof(argv[++i]));
        } else if (is("--bar")) {
            o.bar = true;
        } else if (is("--window")) {
            o.bar = false;
        } else if ((is("--bar-height") || is("--height")) && hasValue) {
            o.barHeight = std::clamp(std::atoi(argv[++i]), 128, 2000);
        } else if (is("--size") && i + 2 < argc) {
            o.bar = false;
            o.width = std::clamp(std::atoi(argv[++i]), 256, 8192);
            o.height = std::clamp(std::atoi(argv[++i]), 128, 4096);
        } else if (is("--theme") && hasValue) {
            o.theme = std::max(0, std::atoi(argv[++i]) - 1);
        } else if (is("--grid")) {
            o.grid = true;
        } else if (is("--list")) {
            o.list = true;
        } else if (is("--watch") && hasValue) {
            o.watch = std::max(0, std::atoi(argv[++i]));
        }
    }
    return o;
}

} // namespace

int main(int argc, char** argv) {
#ifdef _WIN32
    mixviz::DesktopBar::enableDpiAwareness();
#endif
    if (!SDL_Init(SDL_INIT_VIDEO)) {
        SDL_Log("SDL_Init failed: %s", SDL_GetError());
        return 1;
    }
    bool snapping = false;
    for (int i = 1; i < argc; ++i) snapping |= !std::strcmp(argv[i], "--snap");
    Options saved;
    if (!snapping) loadSettings(saved); // a snapshot renders exactly what it was asked for
    Options opt = parse(argc, argv, saved);
    // A hidden snapshot window must never reserve a strip of the desktop.
    if (snapping) opt.bar = false;
#ifndef _WIN32
    opt.bar = false;
#endif

    SDL_WindowFlags flags = opt.bar ? (SDL_WINDOW_BORDERLESS | SDL_WINDOW_ALWAYS_ON_TOP | SDL_WINDOW_HIGH_PIXEL_DENSITY)
                                    : (SDL_WINDOW_RESIZABLE | SDL_WINDOW_HIGH_PIXEL_DENSITY);
    if (snapping) flags |= SDL_WINDOW_HIDDEN;
    SDL_Window* win = nullptr;
    SDL_Renderer* ren = nullptr;
    if (!SDL_CreateWindowAndRenderer("Futureboard Visualizer", opt.bar ? 1280 : opt.width,
                                     opt.bar ? opt.barHeight : opt.height, flags, &win, &ren)) {
        SDL_Log("Window/renderer failed: %s", SDL_GetError());
        SDL_Quit();
        return 1;
    }
    SDL_SetRenderVSync(ren, 1);

#ifdef _WIN32
    mixviz::DesktopBar bar;
#endif
    // Docks the window as a top-of-desktop toolbar, or back to a normal
    // window. Returns whether it is docked.
    auto setDocked = [&](bool dock) {
#ifdef _WIN32
        if (dock) {
            SDL_SetWindowBordered(win, false);
            SDL_SetWindowResizable(win, false);
            SDL_SetWindowAlwaysOnTop(win, true);
            if (bar.attach(win, opt.barHeight)) return true;
            SDL_Log("AppBar registration failed, running as a normal window");
        }
        bar.detach();
#endif
        SDL_SetWindowAlwaysOnTop(win, false);
        SDL_SetWindowBordered(win, true);
        SDL_SetWindowResizable(win, true);
        if (dock) return false;
        SDL_SetWindowSize(win, opt.width, opt.height);
        SDL_SetWindowPosition(win, SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED);
        return false;
    };
    if (opt.bar && !setDocked(true)) opt.bar = false;

    int canvasW = std::max(kMinW, opt.width * kH / std::max(1, opt.height));
    lcd::Canvas canvas(canvasW, kH);
    lcd::LcdRenderer lcdr(ren, canvasW, kH);
    const auto& themes = lcd::builtinThemes();
    size_t themeIdx = static_cast<size_t>(opt.theme) % themes.size();
    lcdr.setTheme(themes[themeIdx]);
    lcdr.setResponseTime(0.03f);
    lcdr.setGrid(opt.grid);
    lcdr.setFill(true); // the canvas follows the window's aspect, edge to edge

    fblink::Link link;
    Session session;
    Analysis master, track;
    Toast toast;
    std::vector<HitColumn> hits;
    PageInfo pageInfo;
    std::array<int, kTabCount> pages{};

    Tab tab = opt.tab;
    bool listMode = opt.list;
    bool follow = true;
    bool frozen = false;
    bool help = false;
    float scopeMs = 20.0f;
    float elapsed = 0.0f;
    float retryIn = 0.0f;
    int sampleRate = 0;
    uint64_t trackAnalysisKey = 0;
    int shot = 0;
    float mouseX = -1.0f, mouseY = -1.0f;
    std::vector<float> bufL(8192), bufR(8192);

    auto stepTab = [&](int delta) {
        tab = static_cast<Tab>((static_cast<int>(tab) + delta + kTabCount) % kTabCount);
    };
    auto focusStep = [&](int delta) {
        if (session.tracks.empty()) return;
        int index = session.indexOf(session.focusKey);
        index = index < 0 ? 0 : std::clamp(index + delta, 0, static_cast<int>(session.tracks.size()) - 1);
        session.focusKey = session.tracks[static_cast<size_t>(index)].key;
        follow = false;
        if (pageInfo.perPage > 0) pages[static_cast<size_t>(tab)] = index / pageInfo.perPage;
    };
    auto canvasPoint = [&](float wx, float wy, int& cx, int& cy) {
        int ww = 0, wh = 0;
        SDL_GetWindowSize(win, &ww, &wh);
        if (ww <= 0 || wh <= 0) return false;
        cx = static_cast<int>(wx * canvasW / ww);
        cy = static_cast<int>(wy * kH / wh);
        return true;
    };

    Uint64 last = SDL_GetTicksNS();
    bool running = true;
    while (running) {
        SDL_Event e;
        while (SDL_PollEvent(&e)) {
            switch (e.type) {
            case SDL_EVENT_QUIT: running = false; break;
            case SDL_EVENT_MOUSE_MOTION:
                mouseX = e.motion.x;
                mouseY = e.motion.y;
                break;
            case SDL_EVENT_WINDOW_MOUSE_LEAVE: mouseX = mouseY = -1.0f; break;
            case SDL_EVENT_MOUSE_WHEEL:
                pages[static_cast<size_t>(tab)] += e.wheel.y > 0 ? -1 : 1;
                break;
            case SDL_EVENT_MOUSE_BUTTON_DOWN: {
                int cx = 0, cy = 0;
                if (e.button.button != SDL_BUTTON_LEFT || !canvasPoint(e.button.x, e.button.y, cx, cy)) break;
                if (cy < kHeaderH) {
                    stepTab(cx < canvasW / 2 ? -1 : 1);
                    break;
                }
                for (const auto& h : hits) {
                    if (cx >= h.x0 && cx <= h.x1 && cy >= h.y0 && cy <= h.y1) {
                        session.focusKey = h.key;
                        follow = false;
                        toast.show("WATCHING " + lcdText(fblink::nameOf(session.find(h.key)->name, fblink::kNameBytes), 24));
                    }
                }
                break;
            }
            case SDL_EVENT_KEY_DOWN: {
                const SDL_Keycode k = e.key.key;
                if (k >= SDLK_1 && k <= SDLK_9) {
                    tab = static_cast<Tab>(k - SDLK_1);
                    break;
                }
                switch (k) {
                case SDLK_ESCAPE:
                    if (help) help = false;
                    else running = false;
                    break;
                case SDLK_F1: help = !help; break;
                case SDLK_TAB: stepTab((e.key.mod & SDL_KMOD_SHIFT) ? -1 : 1); break;
                case SDLK_LEFT: case SDLK_PAGEUP: --pages[static_cast<size_t>(tab)]; break;
                case SDLK_RIGHT: case SDLK_PAGEDOWN: ++pages[static_cast<size_t>(tab)]; break;
                case SDLK_UP:
                case SDLK_DOWN: {
                    const int delta = k == SDLK_UP ? -1 : 1;
                    if (!(e.key.mod & SDL_KMOD_CTRL)) {
                        focusStep(delta);
                        break;
                    }
                    // Ctrl+Up shrinks the strip, Ctrl+Down grows it (the edge
                    // that moves is the bottom one).
                    opt.barHeight = stepBarHeight(opt.barHeight, delta);
#ifdef _WIN32
                    if (opt.bar) bar.setHeight(opt.barHeight);
#endif
                    toast.show("BAR HEIGHT " + std::to_string(opt.barHeight) + " PX");
                    if (!snapping) saveSettings(opt.barHeight, tab, themeIdx);
                    break;
                }
                case SDLK_F:
                    follow = !follow;
                    toast.show(follow ? "FOLLOWING STUDIO SELECTION" : "FOLLOW OFF");
                    break;
                case SDLK_L: listMode = !listMode; break;
                case SDLK_SPACE: frozen = !frozen; break;
                case SDLK_R:
                    master.resetLoudness();
                    track.resetLoudness();
                    toast.show("LOUDNESS RESET");
                    break;
                case SDLK_S: {
                    const float tilt = master.spectrum().tiltDbPerOct > 0 ? 0.0f : 4.5f;
                    master.spectrum().tiltDbPerOct = track.spectrum().tiltDbPerOct = tilt;
                    toast.show(tilt > 0 ? "TILT 4.5 dB/OCT" : "TILT FLAT");
                    break;
                }
                case SDLK_A: {
                    const bool on = !master.imager().autoGain;
                    master.imager().autoGain = track.imager().autoGain = on;
                    toast.show(on ? "IMAGER AUTO GAIN" : "IMAGER FIXED GAIN");
                    break;
                }
                case SDLK_RIGHTBRACKET: scopeMs = std::min(500.0f, scopeMs * 1.5f); break;
                case SDLK_LEFTBRACKET: scopeMs = std::max(2.0f, scopeMs / 1.5f); break;
                case SDLK_T:
                    themeIdx = (themeIdx + 1) % themes.size();
                    lcdr.setTheme(themes[themeIdx]);
                    toast.show(themes[themeIdx].name);
                    break;
                case SDLK_G: lcdr.setGrid(!lcdr.grid()); break;
                case SDLK_H:
                    lcdr.setResponseTime(lcdr.responseTime() > 0 ? 0.0f : 0.03f);
                    toast.show(lcdr.responseTime() > 0 ? "GHOSTING ON" : "GHOSTING OFF");
                    break;
                case SDLK_EQUALS: case SDLK_KP_PLUS: lcdr.setBrightness(lcdr.brightness() + 0.1f); break;
                case SDLK_MINUS: case SDLK_KP_MINUS: lcdr.setBrightness(lcdr.brightness() - 0.1f); break;
                case SDLK_F11:
                    if (!opt.bar) SDL_SetWindowFullscreen(win, (SDL_GetWindowFlags(win) & SDL_WINDOW_FULLSCREEN) == 0);
                    break;
                case SDLK_B:
#ifdef _WIN32
                    if (!snapping) {
                        if (SDL_GetWindowFlags(win) & SDL_WINDOW_FULLSCREEN) SDL_SetWindowFullscreen(win, false);
                        opt.bar = setDocked(!opt.bar);
                        toast.show(opt.bar ? "DOCKED TO THE TOP" : "WINDOW");
                    }
#endif
                    break;
                case SDLK_P: {
                    const std::string path = "fbvis_" + std::to_string(shot++) + ".bmp";
                    toast.show(lcdr.saveBMP(path) ? "SAVED " + path : "SAVE FAILED");
                    break;
                }
                default: break;
                }
                break;
            }
            default: break;
            }
        }

#ifdef _WIN32
        if (int bx, by, bw, bh; bar.takeSlot(bx, by, bw, bh)) {
            SDL_SetWindowPosition(win, bx, by);
            SDL_SetWindowSize(win, bw, bh);
        }
#endif

        const Uint64 now = SDL_GetTicksNS();
        const float dt = std::min(static_cast<float>(now - last) * 1e-9f, 0.25f);
        last = now;
        elapsed += dt;
        toast.timeLeft -= dt;

        // ---- Link: connect, stay connected only while Studio is alive.
        const uint64_t wall = fblink::nowMs();
        if (link.isOpen() && !link.writerAlive(wall)) {
            link.close(); // let a restarted Studio publish a fresh region
            session.connected = false;
            toast.show("STUDIO STOPPED PUBLISHING", 2.0f);
        }
        if (!link.isOpen() && (retryIn -= dt) <= 0.0f) {
            retryIn = 1.0f;
            if (link.open(opt.linkName) && link.writerAlive(wall)) {
                toast.show("CONNECTED TO STUDIO", 1.5f);
                master.reset(0);
                track.reset(0);
                sampleRate = 0;
                trackAnalysisKey = 0;
            } else {
                link.close();
            }
        }
        if (link.isOpen()) {
            link.stampHeartbeat();
            if (link.snapshot(session.frame, session.tracks)) session.connected = true;
        }
        session.updateMeters(dt);

        // ---- Which track's audio to watch.
        if (session.connected && opt.watch > 0 && static_cast<size_t>(opt.watch) <= session.tracks.size()) {
            session.focusKey = session.tracks[static_cast<size_t>(opt.watch - 1)].key;
            follow = false;
            opt.watch = 0;
        }
        if (session.connected) {
            const uint64_t selected = session.frame.selected_track_key;
            if (follow && selected && session.find(selected)) session.focusKey = selected;
            if (session.focusKey && !session.find(session.focusKey)) session.focusKey = 0;
            link.requestTrack(session.focusKey);
        }

        // ---- Audio.
        const int rate = static_cast<int>(link.sampleRate());
        if (rate != sampleRate) {
            sampleRate = rate;
            master.reset(rate);
            track.reset(rate);
        }
        const uint64_t tapped = link.tappedTrackKey();
        if (tapped != trackAnalysisKey) {
            trackAnalysisKey = tapped;
            track.reset(rate);
        }
        if (link.isOpen()) {
            uint64_t skipped = 0;
            size_t n;
            while ((n = link.readMaster(bufL.data(), bufR.data(), bufL.size(), skipped)) > 0)
                master.push(bufL.data(), bufR.data(), n);
            while ((n = link.readTrack(bufL.data(), bufR.data(), bufL.size(), skipped)) > 0)
                if (tapped) track.push(bufL.data(), bufR.data(), n);
        }
        const mixviz::FrameData& masterFd = master.step(dt, scopeMs);
        const mixviz::FrameData& trackFd = track.step(dt, scopeMs);

        // ---- Canvas follows the window's aspect.
        int ow = 0, oh = 0;
        SDL_GetRenderOutputSize(ren, &ow, &oh);
        if (oh > 0) {
            const int want = std::max(kMinW, static_cast<int>(std::lround(static_cast<double>(kH) * ow / oh)));
            if (want != canvasW) {
                canvasW = want;
                canvas = lcd::Canvas(canvasW, kH);
                lcdr.resize(canvasW, kH);
            }
        }

        // ---- Hover.
        session.hoverKey = 0;
        if (int cx, cy; mouseX >= 0 && canvasPoint(mouseX, mouseY, cx, cy)) {
            for (const auto& h : hits)
                if (cx >= h.x0 && cx <= h.x1 && cy >= h.y0 && cy <= h.y1) session.hoverKey = h.key;
        }

        // ---- Draw.
        if (!frozen) {
            canvas.clear();
            const Rect body{0, kHeaderH + 1, canvasW, kH - kHeaderH - 1};
            const int tabIndex = static_cast<int>(tab);
            std::string title = std::to_string(tabIndex + 1) + " " + kTabNames[tabIndex];
            if (tab != Tab::Meters && tab != Tab::Mixer) hits.clear();
            const bool audioTab = tab != Tab::Meters && tab != Tab::Mixer && tab != Tab::Transport;
            if (!session.connected) {
                drawHeader(canvas, 0, canvasW, title, "NO STUDIO");
                drawCentered(canvas, canvasW, "WAITING FOR FUTUREBOARD STUDIO", link.lastError());
            } else {
                switch (tab) {
                case Tab::Meters:
                    drawMeters(canvas, body, session, listMode, pages[0], hits, pageInfo);
                    break;
                case Tab::Mixer:
                    drawMixer(canvas, body, session, pages[1], hits, pageInfo);
                    break;
                case Tab::Transport: drawTransport(canvas, body, session, sampleRate); break;
                default: break;
                }
                if (audioTab && tab != Tab::Track && !masterFd.loudness) {
                    drawCentered(canvas, canvasW, "NO MASTER AUDIO YET", "PLAY SOMETHING IN STUDIO");
                } else if (tab == Tab::Track) {
                    const auto* focused = session.find(session.focusKey);
                    if (focused) title += " " + lcdText(fblink::nameOf(focused->name, fblink::kNameBytes), 20);
                    if (!focused) {
                        drawCentered(canvas, canvasW, "NO TRACK SELECTED",
                                     "SELECT ONE IN STUDIO, OR UP/DOWN HERE");
                    } else if (tapped != session.focusKey || !trackFd.loudness) {
                        drawCentered(canvas, canvasW, "WAITING FOR THE TRACK'S AUDIO", "");
                    } else {
                        drawQuad(canvas, canvasW, trackFd, track.imager(),
                                 "8 " + lcdText(fblink::nameOf(focused->name, fblink::kNameBytes), 12),
                                 statusText(session, false));
                        if (!track.live()) canvas.textCentered(canvasW / 4, kH - 9, "SILENT", 1, kMid);
                    }
                } else if (audioTab) {
                    switch (tab) {
                    case Tab::Loudness: drawLoudness(canvas, body, masterFd, false); break;
                    case Tab::Spectrum: drawSpectrum(canvas, body, masterFd, false); break;
                    case Tab::Stereo: drawImager(canvas, body, master.imager(), masterFd, false); break;
                    case Tab::Scope: drawScope(canvas, body, masterFd, false); break;
                    case Tab::Overview:
                        drawQuad(canvas, canvasW, masterFd, master.imager(), "7 MASTER", statusText(session, false));
                        break;
                    default: break;
                    }
                }
                const bool quad = (tab == Tab::Overview && masterFd.loudness) ||
                                  (tab == Tab::Track && session.find(session.focusKey) &&
                                   tapped == session.focusKey && trackFd.loudness);
                std::string status = statusText(session, false);
                if ((tab == Tab::Meters || tab == Tab::Mixer) && pageInfo.pages > 1)
                    status = "P" + std::to_string(pageInfo.page + 1) + "/" + std::to_string(pageInfo.pages) + "  " + status;
                if (!quad) drawHeader(canvas, 0, canvasW, title, status);
            }
            if (help) drawHelp(canvas, canvasW);
            toast.draw(canvas);
        } else {
            canvas.fillRect(canvasW - 30, kH - 10, 30, 10, 0);
            canvas.text(canvasW - 28, kH - 8, "HOLD", 1, (static_cast<int>(elapsed * 2) & 1) ? 255 : 90);
        }

        lcdr.update(canvas, dt);
        SDL_SetRenderDrawColor(ren, 18, 18, 20, 255);
        SDL_RenderClear(ren);
        lcdr.draw(SDL_FRect{0, 0, static_cast<float>(ow), static_cast<float>(oh)});
        SDL_RenderPresent(ren);

        if (snapping && elapsed > opt.snapAfter) {
            if (!lcdr.saveBMP(opt.snapPath)) SDL_Log("Saving %s failed", opt.snapPath.c_str());
            running = false;
        }
        // A hidden window may not wait for vsync; keep to ~120 fps.
        const Uint64 spent = SDL_GetTicksNS() - now;
        if (spent < 8'000'000) SDL_DelayNS(8'000'000 - spent);
    }

    link.close();
    if (!snapping) saveSettings(opt.barHeight, tab, themeIdx);
#ifdef _WIN32
    bar.detach(); // release the reserved desktop strip before the window goes away
#endif
    SDL_DestroyRenderer(ren);
    SDL_DestroyWindow(win);
    SDL_Quit();
    return 0;
}
