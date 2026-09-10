// editor_mac_shell.mm — the plug-in editor's chrome strip, in AppKit.
//
// # What this is
//
// On Windows the plug-in's view is a child window inside the studio's own GPUI
// window, so GPUI draws the editor chrome — the tab strip and the row of
// controls under it — directly above the plug-in's surface. macOS cannot put a
// view inside another process's window, so the editor window belongs to the
// plug-in host process. The chrome has to be drawn where the window is, which
// is here.
//
// This is a port of `components/plugin_editor_chrome.rs`, not a second design:
// same two bands at the same heights, same controls in the same order, same
// colours (they arrive resolved from the studio's theme — see
// `EditorChromePalette`). What is *not* ported is anything the studio decides.
// Nothing here reads a preset from disk, knows whether an insert is bypassed,
// or formats a latency: every string is handed over finished and every press
// goes back as an action for the studio to apply. That is the same contract the
// GPUI chrome has with `StudioLayout`, kept across a process boundary.
//
// # Layout
//
//     ┌───────────────────────────────┐
//     │ native NSWindow title bar      │  the window's own — not drawn here
//     ├───────────────────────────────┤
//     │ tab strip            30 pt     │  DauxEditorChromeView
//     │ chrome row           26 pt     │
//     ├───────────────────────────────┤
//     │                               │
//     │ the plug-in's NSView          │  parented by editor_mac.mm
//     │                               │
//     └───────────────────────────────┘
//
// The container (`DauxEditorShellView`) is the window's content view and owns
// both. `editor_mac.mm` asks it for `pluginArea` and puts the plug-in's embed
// view there, so the plug-in's own geometry never has to know the chrome
// exists.
//
// # Threading
//
// AppKit main thread only, which is also the host process's IPC thread — see
// `host_ui_mac.mm`. The one exception is `editor_mac_chrome_take_action`, which
// is called from the same thread anyway but is written to be safe if that ever
// stops being true: the queue it drains is guarded.

#include "sphere_daux_editor_chrome.h"
#include "sphere_daux_editor_shell_mac.h"

#import <Cocoa/Cocoa.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

/// Band heights, in points. The same numbers as `TAB_STRIP_H` and `CHROME_H` in
/// the GPUI chrome — a tab strip sized to a browser tab, and a control row that
/// fits an 18 pt control with breathing room.
constexpr CGFloat kTabStripHeight = 30.0;
constexpr CGFloat kChromeRowHeight = 26.0;

/// Control metrics, mirroring `plugin_editor_chrome.rs`.
constexpr CGFloat kControlHeight = 18.0;
constexpr CGFloat kIconButtonWidth = 22.0;
constexpr CGFloat kPresetTriggerMinWidth = 120.0;
constexpr CGFloat kRowPadX = 8.0;
constexpr CGFloat kRowGap = 10.0;
constexpr CGFloat kStepGap = 2.0;
constexpr CGFloat kControlRadius = 3.0;
constexpr CGFloat kTabRadius = 5.0;
constexpr CGFloat kTabMaxWidth = 220.0;
constexpr CGFloat kTabGap = 2.0;
constexpr CGFloat kTabStripPadX = 4.0;
constexpr CGFloat kTabCloseSide = 16.0;

/// Type sizes, mirroring the GPUI chrome's `UI_XS` / tab label sizes.
constexpr CGFloat kLabelSize = 11.0;
constexpr CGFloat kReadoutSize = 10.0;
constexpr CGFloat kTabNumberSize = 10.0;

/// Which control a hit landed on. `-1` is "nothing".
enum ChromeHit {
  kHitNone = -1,
  kHitPower = 0,
  kHitPresetPrev,
  kHitPresetTrigger,
  kHitPresetNext,
  kHitPresetSave,
  /// Tabs and their close buttons are `kHitTabBase + index * 2 (+ 1)`.
  kHitTabBase = 100,
};

/// Mirrors `EditorChromeCommand`'s discriminants. The Rust side turns these
/// back into the same enum the GPUI chrome queues, so the numbering is a wire
/// contract — append only.
enum ChromeActionKind {
  kActionSetActive = 0,
  kActionStepPreset = 1,
  kActionSavePreset = 2,
  kActionSelectPreset = 3,
  kActionSelectTab = 4,
  kActionCloseTab = 5,
};

struct ChromeAction {
  int kind;
  int value;
  std::string insert_id;
};

struct ChromeTab {
  std::string insert_id;
  std::string display_name;
  int insert_number;
};

/// The resolved theme colours, in the order the Rust `EditorChromePalette`
/// packs them. Indices are a wire contract with
/// `editor_mac_chrome_set_palette`.
enum PaletteSlot {
  kPaletteStripBg = 0,
  kPaletteRowBg,
  kPaletteBorder,
  kPaletteControlBg,
  kPaletteControlHover,
  kPaletteControlPressed,
  kPaletteAccent,
  kPaletteTextPrimary,
  kPaletteTextSecondary,
  kPaletteTextFaint,
  kPaletteCount,
};

/// Dark-theme values, used only until the first `SetEditorChrome` lands.
///
/// Not a second palette to maintain: the window is created before the studio
/// has said anything about it, and one frame of the wrong grey is worse than
/// one frame of a reasonable one. Every value here is replaced by the theme's
/// on the first update.
constexpr unsigned int kPaletteFallback[kPaletteCount] = {
    0x1B1D22FFu, // strip
    0x212429FFu, // row
    0x000000FFu, // border — alpha-less black reads as the hairline the theme uses
    0x16181CFFu, // control
    0x2A2E35FFu, // control hover
    0x101216FFu, // control pressed
    0x4FC9D8FFu, // accent
    0xE8EAEEFFu, // text primary
    0xB9BDC6FFu, // text secondary
    0x8F949FFFu, // text faint
};

NSColor *color_from_rgba(unsigned int rgba) {
  const CGFloat r = (CGFloat)((rgba >> 24) & 0xFFu) / 255.0;
  const CGFloat g = (CGFloat)((rgba >> 16) & 0xFFu) / 255.0;
  const CGFloat b = (CGFloat)((rgba >> 8) & 0xFFu) / 255.0;
  const CGFloat a = (CGFloat)(rgba & 0xFFu) / 255.0;
  return [NSColor colorWithSRGBRed:r green:g blue:b alpha:a];
}

NSString *ns(const std::string &value) {
  NSString *s = [NSString stringWithUTF8String:value.c_str()];
  return s ? s : @"";
}

/// One SF Symbol, tinted.
///
/// Returns nil when the symbol is unavailable, and every draw site treats nil
/// as "draw nothing" rather than substituting a placeholder box — a chrome with
/// one missing glyph is better than one with a stand-in nobody recognises.
///
/// # Why `respondsToSelector:` and not `@available`
///
/// `@available` compiles to a call to `___isPlatformVersionAtLeast`, a
/// compiler-rt builtin. Rust links this bridge itself and does not pull clang's
/// runtime library in, so on x86_64 that symbol is simply undefined and the
/// whole application fails to link. It happens to resolve on arm64, which is
/// exactly how a build that only ever ran on Apple silicon shipped a link error
/// for Intel.
///
/// Asking the class whether it answers the selector needs no runtime library,
/// says the same thing, and says it about the API actually being called rather
/// than about an OS version standing in for it.
NSImage *chrome_symbol(NSString *name, NSColor *tint, CGFloat size) {
  if (![NSImage respondsToSelector:@selector(imageWithSystemSymbolName:
                                                accessibilityDescription:)]) {
    return nil;
  }
  NSImage *image = [NSImage imageWithSystemSymbolName:name
                             accessibilityDescription:nil];
  if (!image) {
    return nil;
  }
  NSImageSymbolConfiguration *config =
      [NSImageSymbolConfiguration configurationWithPointSize:size
                                                      weight:NSFontWeightRegular];
  if ([NSImageSymbolConfiguration
          respondsToSelector:@selector(configurationWithHierarchicalColor:)]) {
    // Colour through the symbol configuration rather than by filling a copy:
    // the fill route needs `lockFocus`, which rasterises at one scale and then
    // looks soft on a display with a different one.
    NSImageSymbolConfiguration *tinted = [NSImageSymbolConfiguration
        configurationWithHierarchicalColor:tint];
    config = [config configurationByApplyingConfiguration:tinted];
  }
  return [image imageWithSymbolConfiguration:config];
}

void draw_text(NSString *text, NSRect rect, NSColor *color, CGFloat size,
               NSTextAlignment alignment) {
  if (text.length == 0) {
    return;
  }
  NSMutableParagraphStyle *style =
      [[NSParagraphStyle defaultParagraphStyle] mutableCopy];
  style.alignment = alignment;
  style.lineBreakMode = NSLineBreakByTruncatingTail;
  NSDictionary *attrs = @{
    NSFontAttributeName : [NSFont systemFontOfSize:size],
    NSForegroundColorAttributeName : color,
    NSParagraphStyleAttributeName : style,
  };
  // Vertically centred by hand: `drawInRect:` top-aligns, and a chrome row is
  // short enough that a few points of drift reads as a misaligned control.
  const CGFloat text_height = [text sizeWithAttributes:attrs].height;
  NSRect centred = rect;
  centred.origin.y += (rect.size.height - text_height) / 2.0;
  centred.size.height = text_height;
  [text drawInRect:centred withAttributes:attrs];
}

CGFloat text_width(NSString *text, CGFloat size) {
  if (text.length == 0) {
    return 0.0;
  }
  NSDictionary *attrs = @{NSFontAttributeName : [NSFont systemFontOfSize:size]};
  return [text sizeWithAttributes:attrs].width;
}

void fill_rounded(NSRect rect, CGFloat radius, NSColor *color) {
  if (rect.size.width <= 0.0 || rect.size.height <= 0.0) {
    return;
  }
  [color set];
  [[NSBezierPath bezierPathWithRoundedRect:rect
                                   xRadius:radius
                                   yRadius:radius] fill];
}

} // namespace

// ── The chrome view ──────────────────────────────────────────────────────────

@interface DauxEditorChromeView : NSView
/// Only for the commit log below — the strip's own drawing reads the vectors.
- (NSInteger)tabCount;
- (NSInteger)presetCount;
@end

@implementation DauxEditorChromeView {
  unsigned int _palette[kPaletteCount];
  std::string _presetLabel;
  std::string _cpuLabel;
  std::string _latencyLabel;
  std::vector<std::string> _presets;
  std::vector<ChromeTab> _tabs;
  std::string _activeTab;
  int _presetIndex;
  BOOL _active;
  BOOL _hasPresets;

  // Staging for the builder ABI: a chrome update arrives as several calls and
  // must not be half-applied into what is on screen.
  std::vector<std::string> _pendingPresets;
  std::vector<ChromeTab> _pendingTabs;

  std::vector<ChromeAction> _actions;

  int _hot;     // control under the pointer
  int _pressed; // control the mouse went down on

  /// The strip's height when the staged update began — see `-beginUpdate`.
  CGFloat _heightAtUpdateStart;

  /// Whether this editor has an insert behind it to control.
  ///
  /// An ARA plug-in is bound to a clip, not to an insert slot: it has no
  /// bypass, no per-slot CPU or latency, and no insert-keyed preset list. The
  /// row is dropped entirely rather than drawn with controls that would do
  /// nothing — a control that cannot do what it says is worse than no control.
  BOOL _showsControls;

  // Rects from the last layout pass, in this view's coordinates. Drawing and
  // hit-testing read the same ones, so a control can never be somewhere other
  // than where it is clickable.
  NSRect _powerRect;
  NSRect _prevRect;
  NSRect _triggerRect;
  NSRect _nextRect;
  NSRect _saveRect;
  std::vector<NSRect> _tabRects;
  std::vector<NSRect> _tabCloseRects;
}

- (instancetype)initWithFrame:(NSRect)frame {
  self = [super initWithFrame:frame];
  if (self) {
    for (int i = 0; i < kPaletteCount; ++i) {
      _palette[i] = kPaletteFallback[i];
    }
    _showsControls = YES;
    _heightAtUpdateStart = kTabStripHeight + kChromeRowHeight;
    _presetLabel = "No presets";
    _cpuLabel = "—";
    _latencyLabel = "0 ms";
    _presetIndex = -1;
    _active = YES;
    _hasPresets = NO;
    _hot = kHitNone;
    _pressed = kHitNone;
    self.wantsLayer = YES;
  }
  return self;
}

/// Top-left origin, like the GPUI chrome it mirrors. Every rect below is
/// written the way the Rust is, which is what makes the two readable side by
/// side.
- (BOOL)isFlipped {
  return YES;
}

/// How tall this strip is right now — the tab band, plus the control row when
/// there is an insert behind it.
- (CGFloat)chromeHeight {
  return kTabStripHeight + (_showsControls ? kChromeRowHeight : 0.0);
}

- (BOOL)showsControls {
  return _showsControls;
}

- (NSColor *)slot:(int)index {
  return color_from_rgba(_palette[index]);
}

// ── Chrome updates ─────────────────────────────────────────────────────────

- (void)beginUpdate {
  // Remembered per strip, not in a file-static: two editors can be open, and
  // one adopting the other's height is a window that jumps for no reason.
  _heightAtUpdateStart = [self chromeHeight];
  _pendingPresets.clear();
  _pendingTabs.clear();
}

/// How tall this strip was when the update now being staged began.
- (CGFloat)heightAtUpdateStart {
  return _heightAtUpdateStart;
}

- (void)setHeaderActive:(BOOL)active
            presetLabel:(const char *)presetLabel
               cpuLabel:(const char *)cpuLabel
           latencyLabel:(const char *)latencyLabel
              activeTab:(const char *)activeTab
          showsControls:(BOOL)showsControls {
  _showsControls = showsControls;
  _active = active;
  _presetLabel = presetLabel ? presetLabel : "";
  _cpuLabel = cpuLabel ? cpuLabel : "";
  _latencyLabel = latencyLabel ? latencyLabel : "";
  _activeTab = activeTab ? activeTab : "";
}

- (void)addPreset:(const char *)name selected:(BOOL)selected {
  if (selected) {
    _presetIndex = (int)_pendingPresets.size();
  }
  _pendingPresets.push_back(name ? name : "");
}

- (void)addTab:(const char *)insertId
          name:(const char *)displayName
        number:(int)insertNumber {
  ChromeTab tab;
  tab.insert_id = insertId ? insertId : "";
  tab.display_name = displayName ? displayName : "";
  tab.insert_number = insertNumber;
  _pendingTabs.push_back(std::move(tab));
}

- (void)setPalette:(const unsigned int *)colors count:(int)count {
  if (!colors) {
    return;
  }
  const int limit = count < kPaletteCount ? count : kPaletteCount;
  for (int i = 0; i < limit; ++i) {
    _palette[i] = colors[i];
  }
}

- (void)commitUpdate {
  // `_presetIndex` was rebuilt by `addPreset:selected:` during staging; a
  // commit with no selected row means nothing is loaded.
  if (_pendingPresets.empty()) {
    _presetIndex = -1;
  } else if (_presetIndex >= (int)_pendingPresets.size()) {
    _presetIndex = -1;
  }
  _presets = _pendingPresets;
  _tabs = _pendingTabs;
  _hasPresets = !_presets.empty();
  _pendingPresets.clear();
  _pendingTabs.clear();
  [self setNeedsDisplay:YES];
}

- (void)resetPresetIndex {
  _presetIndex = -1;
}

- (NSInteger)tabCount {
  return (NSInteger)_tabs.size();
}

- (NSInteger)presetCount {
  return (NSInteger)_presets.size();
}

// ── Actions ────────────────────────────────────────────────────────────────

- (void)queueKind:(int)kind value:(int)value insertId:(const std::string &)id {
  ChromeAction action;
  action.kind = kind;
  action.value = value;
  action.insert_id = id;
  _actions.push_back(std::move(action));
}

- (BOOL)takeAction:(int *)outKind
             value:(int *)outValue
                id:(char *)outId
          capacity:(int)capacity {
  if (_actions.empty()) {
    return NO;
  }
  const ChromeAction action = _actions.front();
  _actions.erase(_actions.begin());
  if (outKind) {
    *outKind = action.kind;
  }
  if (outValue) {
    *outValue = action.value;
  }
  if (outId && capacity > 0) {
    const size_t n = action.insert_id.size() < (size_t)(capacity - 1)
                         ? action.insert_id.size()
                         : (size_t)(capacity - 1);
    std::memcpy(outId, action.insert_id.data(), n);
    outId[n] = '\0';
  }
  return YES;
}

// ── Layout ─────────────────────────────────────────────────────────────────

/// Recompute every control rect. Called from `drawRect:` and from the hit
/// tests, so the two can never be looking at different geometry.
- (void)layoutControls {
  const CGFloat width = self.bounds.size.width;
  const CGFloat rowTop = kTabStripHeight;
  const CGFloat controlY = rowTop + (kChromeRowHeight - kControlHeight) / 2.0;

  CGFloat x = kRowPadX;
  _powerRect = NSMakeRect(x, controlY, kIconButtonWidth, kControlHeight);
  x = NSMaxX(_powerRect) + kRowGap;

  _prevRect = NSMakeRect(x, controlY, kIconButtonWidth, kControlHeight);
  x = NSMaxX(_prevRect) + kStepGap;

  // The trigger grows with its label so a long preset name is not permanently
  // truncated, but never so far that the readouts get pushed off the row.
  const CGFloat labelWidth = text_width(ns(_presetLabel), kLabelSize);
  CGFloat triggerWidth = labelWidth + 34.0;
  if (triggerWidth < kPresetTriggerMinWidth) {
    triggerWidth = kPresetTriggerMinWidth;
  }
  const CGFloat triggerMax = width * 0.4;
  if (triggerWidth > triggerMax && triggerMax > kPresetTriggerMinWidth) {
    triggerWidth = triggerMax;
  }
  _triggerRect = NSMakeRect(x, controlY, triggerWidth, kControlHeight);
  x = NSMaxX(_triggerRect) + kStepGap;

  _nextRect = NSMakeRect(x, controlY, kIconButtonWidth, kControlHeight);
  x = NSMaxX(_nextRect) + kStepGap;

  const CGFloat saveWidth = text_width(@"Save", kLabelSize) + 16.0;
  _saveRect = NSMakeRect(x, controlY, saveWidth, kControlHeight);

  // Tabs, left to right, clipped to the strip.
  _tabRects.clear();
  _tabCloseRects.clear();
  CGFloat tabX = kTabStripPadX;
  const CGFloat tabH = kTabStripHeight - 4.0;
  const CGFloat tabY = kTabStripHeight - tabH;
  for (const ChromeTab &tab : _tabs) {
    const CGFloat nameWidth = text_width(ns(tab.display_name), kLabelSize);
    CGFloat tabW = nameWidth + 52.0;
    if (tabW > kTabMaxWidth) {
      tabW = kTabMaxWidth;
    }
    NSRect rect = NSMakeRect(tabX, tabY, tabW, tabH);
    _tabRects.push_back(rect);
    _tabCloseRects.push_back(
        NSMakeRect(NSMaxX(rect) - kTabCloseSide - 4.0,
                   tabY + (tabH - kTabCloseSide) / 2.0, kTabCloseSide,
                   kTabCloseSide));
    tabX = NSMaxX(rect) + kTabGap;
  }
}

// ── Drawing ────────────────────────────────────────────────────────────────

- (void)drawRect:(NSRect)dirty {
  (void)dirty;
  [self layoutControls];

  const CGFloat width = self.bounds.size.width;
  NSColor *border = [self slot:kPaletteBorder];

  // Bands.
  [[self slot:kPaletteStripBg] set];
  NSRectFill(NSMakeRect(0, 0, width, kTabStripHeight));
  [border set];
  NSRectFill(NSMakeRect(0, kTabStripHeight - 1.0, width, 1.0));
  if (_showsControls) {
    [[self slot:kPaletteRowBg] set];
    NSRectFill(NSMakeRect(0, kTabStripHeight, width, kChromeRowHeight));
    [border set];
    NSRectFill(NSMakeRect(0, kTabStripHeight + kChromeRowHeight - 1.0, width,
                          1.0));
  }

  [self drawTabs];
  if (_showsControls) {
    [self drawControls];
  }
}

- (void)drawTabs {
  NSColor *textPrimary = [self slot:kPaletteTextPrimary];
  NSColor *textSecondary = [self slot:kPaletteTextSecondary];
  NSColor *textFaint = [self slot:kPaletteTextFaint];

  for (size_t i = 0; i < _tabs.size() && i < _tabRects.size(); ++i) {
    const ChromeTab &tab = _tabs[i];
    const NSRect rect = _tabRects[i];
    const BOOL selected = tab.insert_id == _activeTab;
    const int tabHit = kHitTabBase + (int)i * 2;
    const BOOL hovered = _hot == tabHit;

    NSColor *fill;
    if (selected) {
      fill = [self slot:kPaletteRowBg];
    } else if (hovered) {
      fill = [self slot:kPaletteControlHover];
    } else {
      fill = [self slot:kPaletteStripBg];
    }
    // Only the top corners round: a tab meets the row below it flush, which is
    // what makes the selected one read as continuous with the chrome.
    NSBezierPath *path = [NSBezierPath bezierPath];
    [path moveToPoint:NSMakePoint(NSMinX(rect), NSMaxY(rect))];
    [path lineToPoint:NSMakePoint(NSMinX(rect), NSMinY(rect) + kTabRadius)];
    [path appendBezierPathWithArcFromPoint:NSMakePoint(NSMinX(rect), NSMinY(rect))
                                   toPoint:NSMakePoint(NSMinX(rect) + kTabRadius,
                                                       NSMinY(rect))
                                    radius:kTabRadius];
    [path lineToPoint:NSMakePoint(NSMaxX(rect) - kTabRadius, NSMinY(rect))];
    [path appendBezierPathWithArcFromPoint:NSMakePoint(NSMaxX(rect), NSMinY(rect))
                                   toPoint:NSMakePoint(NSMaxX(rect),
                                                       NSMinY(rect) + kTabRadius)
                                    radius:kTabRadius];
    [path lineToPoint:NSMakePoint(NSMaxX(rect), NSMaxY(rect))];
    [path closePath];
    [fill set];
    [path fill];
    if (selected) {
      [[self slot:kPaletteBorder] set];
      [path setLineWidth:1.0];
      [path stroke];
    }

    // The slot number leads: on a channel with two of the same plug-in, the
    // name alone does not say which one this is.
    NSRect numberRect = NSMakeRect(NSMinX(rect) + 8.0, NSMinY(rect), 14.0,
                                   rect.size.height);
    draw_text([NSString stringWithFormat:@"%d", tab.insert_number], numberRect,
              textFaint, kTabNumberSize, NSTextAlignmentLeft);

    NSRect nameRect =
        NSMakeRect(NSMaxX(numberRect) + 2.0, NSMinY(rect),
                   NSMinX(_tabCloseRects[i]) - NSMaxX(numberRect) - 6.0,
                   rect.size.height);
    draw_text(ns(tab.display_name), nameRect,
              selected ? textPrimary : textSecondary, kLabelSize,
              NSTextAlignmentLeft);

    const NSRect closeRect = _tabCloseRects[i];
    if (_hot == tabHit + 1) {
      fill_rounded(closeRect, kControlRadius, [self slot:kPaletteControlHover]);
    }
    NSImage *glyph = chrome_symbol(@"xmark", textFaint, 8.0);
    if (glyph) {
      NSRect target = NSMakeRect(
          NSMidX(closeRect) - glyph.size.width / 2.0,
          NSMidY(closeRect) - glyph.size.height / 2.0, glyph.size.width,
          glyph.size.height);
      [glyph drawInRect:target
               fromRect:NSZeroRect
              operation:NSCompositingOperationSourceOver
               fraction:1.0
         respectFlipped:YES
                  hints:nil];
    }
  }
}

- (void)drawIconButton:(NSRect)rect
                symbol:(NSString *)name
               enabled:(BOOL)enabled
                  onOff:(BOOL)latched
                   hit:(int)hit {
  NSColor *tint;
  if (!enabled) {
    tint = [self slot:kPaletteTextFaint];
  } else if (latched) {
    tint = [self slot:kPaletteTextPrimary];
  } else {
    tint = [self slot:kPaletteTextSecondary];
  }
  if (enabled && latched) {
    // A latched control carries its state on the fill *and* the glyph colour,
    // never on one channel alone.
    fill_rounded(rect, kControlRadius, [self slot:kPaletteAccent]);
  } else if (enabled && _pressed == hit) {
    fill_rounded(rect, kControlRadius, [self slot:kPaletteControlPressed]);
  } else if (enabled && _hot == hit) {
    fill_rounded(rect, kControlRadius, [self slot:kPaletteControlHover]);
  }
  NSImage *glyph = chrome_symbol(name, tint, 11.0);
  if (!glyph) {
    return;
  }
  NSRect target =
      NSMakeRect(NSMidX(rect) - glyph.size.width / 2.0,
                 NSMidY(rect) - glyph.size.height / 2.0, glyph.size.width,
                 glyph.size.height);
  [glyph drawInRect:target
           fromRect:NSZeroRect
          operation:NSCompositingOperationSourceOver
           fraction:1.0
     respectFlipped:YES
              hints:nil];
}

- (void)drawControls {
  [self drawIconButton:_powerRect
                symbol:@"power"
               enabled:YES
                 onOff:_active
                   hit:kHitPower];
  [self drawIconButton:_prevRect
                symbol:@"chevron.left"
               enabled:_hasPresets
                 onOff:NO
                   hit:kHitPresetPrev];
  [self drawIconButton:_nextRect
                symbol:@"chevron.right"
               enabled:_hasPresets
                 onOff:NO
                   hit:kHitPresetNext];

  // Preset trigger: the name is the menu.
  NSColor *triggerFill;
  if (_pressed == kHitPresetTrigger) {
    triggerFill = [self slot:kPaletteControlPressed];
  } else if (_hot == kHitPresetTrigger) {
    triggerFill = [self slot:kPaletteControlHover];
  } else {
    triggerFill = [self slot:kPaletteControlBg];
  }
  fill_rounded(_triggerRect, kControlRadius, triggerFill);
  NSRect labelRect = NSMakeRect(NSMinX(_triggerRect) + 8.0, NSMinY(_triggerRect),
                                _triggerRect.size.width - 24.0,
                                _triggerRect.size.height);
  draw_text(ns(_presetLabel), labelRect, [self slot:kPaletteTextSecondary],
            kLabelSize, NSTextAlignmentLeft);
  NSImage *chevron = chrome_symbol(@"chevron.down", [self slot:kPaletteTextFaint], 8.0);
  if (chevron) {
    NSRect target = NSMakeRect(NSMaxX(_triggerRect) - 14.0,
                               NSMidY(_triggerRect) - chevron.size.height / 2.0,
                               chevron.size.width, chevron.size.height);
    [chevron drawInRect:target
               fromRect:NSZeroRect
              operation:NSCompositingOperationSourceOver
               fraction:1.0
         respectFlipped:YES
                  hints:nil];
  }

  // Save.
  NSColor *saveFill;
  if (_pressed == kHitPresetSave) {
    saveFill = [self slot:kPaletteControlPressed];
  } else if (_hot == kHitPresetSave) {
    saveFill = [self slot:kPaletteControlHover];
  } else {
    saveFill = [self slot:kPaletteControlBg];
  }
  fill_rounded(_saveRect, kControlRadius, saveFill);
  draw_text(@"Save", _saveRect, [self slot:kPaletteTextSecondary], kLabelSize,
            NSTextAlignmentCenter);

  // Readouts sit at the far end: they are watched, not operated, so they stay
  // clear of the controls the pointer goes for.
  const CGFloat rowTop = kTabStripHeight;
  NSString *cpu = ns(_cpuLabel);
  NSString *latency = ns(_latencyLabel);
  const CGFloat latencyWidth = text_width(latency, kReadoutSize) + 18.0;
  const CGFloat cpuWidth = text_width(cpu, kReadoutSize) + 18.0;
  NSRect latencyRect =
      NSMakeRect(self.bounds.size.width - kRowPadX - latencyWidth, rowTop,
                 latencyWidth, kChromeRowHeight);
  NSRect cpuRect = NSMakeRect(NSMinX(latencyRect) - kRowGap - cpuWidth, rowTop,
                              cpuWidth, kChromeRowHeight);
  // Only drawn when there is room for them: a narrow window would otherwise
  // paint the readouts over the Save button.
  if (NSMinX(cpuRect) > NSMaxX(_saveRect) + kRowGap) {
    [self drawReadout:cpuRect symbol:@"cpu" text:cpu];
    [self drawReadout:latencyRect symbol:@"timer" text:latency];
  }
}

- (void)drawReadout:(NSRect)rect symbol:(NSString *)name text:(NSString *)text {
  NSImage *glyph = chrome_symbol(name, [self slot:kPaletteTextFaint], 9.0);
  CGFloat textX = NSMinX(rect);
  if (glyph) {
    NSRect target =
        NSMakeRect(NSMinX(rect), NSMidY(rect) - glyph.size.height / 2.0,
                   glyph.size.width, glyph.size.height);
    [glyph drawInRect:target
             fromRect:NSZeroRect
            operation:NSCompositingOperationSourceOver
             fraction:1.0
       respectFlipped:YES
                hints:nil];
    textX = NSMaxX(target) + 4.0;
  }
  draw_text(text, NSMakeRect(textX, NSMinY(rect), NSMaxX(rect) - textX,
                             rect.size.height),
            [self slot:kPaletteTextSecondary], kReadoutSize,
            NSTextAlignmentLeft);
}

// ── Hit testing and input ──────────────────────────────────────────────────

- (int)hitAt:(NSPoint)point {
  [self layoutControls];
  // Tabs are always live; the control row only exists when it is drawn, and a
  // hit test that forgot that would fire a bypass nobody can see.
  if (!_showsControls) {
    for (size_t i = 0; i < _tabRects.size(); ++i) {
      if (NSPointInRect(point, _tabCloseRects[i])) {
        return kHitTabBase + (int)i * 2 + 1;
      }
      if (NSPointInRect(point, _tabRects[i])) {
        return kHitTabBase + (int)i * 2;
      }
    }
    return kHitNone;
  }
  if (NSPointInRect(point, _powerRect)) {
    return kHitPower;
  }
  if (_hasPresets && NSPointInRect(point, _prevRect)) {
    return kHitPresetPrev;
  }
  if (NSPointInRect(point, _triggerRect)) {
    return kHitPresetTrigger;
  }
  if (_hasPresets && NSPointInRect(point, _nextRect)) {
    return kHitPresetNext;
  }
  if (NSPointInRect(point, _saveRect)) {
    return kHitPresetSave;
  }
  for (size_t i = 0; i < _tabRects.size(); ++i) {
    // The close button is inside the tab, so it is tested first — otherwise
    // every close would read as a tab select.
    if (NSPointInRect(point, _tabCloseRects[i])) {
      return kHitTabBase + (int)i * 2 + 1;
    }
    if (NSPointInRect(point, _tabRects[i])) {
      return kHitTabBase + (int)i * 2;
    }
  }
  return kHitNone;
}

- (void)updateTrackingAreas {
  [super updateTrackingAreas];
  for (NSTrackingArea *area in [self.trackingAreas copy]) {
    [self removeTrackingArea:area];
  }
  NSTrackingArea *area = [[NSTrackingArea alloc]
      initWithRect:self.bounds
           options:(NSTrackingMouseEnteredAndExited | NSTrackingMouseMoved |
                    NSTrackingActiveInKeyWindow | NSTrackingInVisibleRect)
             owner:self
          userInfo:nil];
  [self addTrackingArea:area];
}

- (void)setHot:(int)hit {
  if (_hot == hit) {
    return;
  }
  _hot = hit;
  [self setNeedsDisplay:YES];
}

- (void)mouseMoved:(NSEvent *)event {
  [self setHot:[self hitAt:[self convertPoint:event.locationInWindow
                                     fromView:nil]]];
}

- (void)mouseExited:(NSEvent *)event {
  (void)event;
  [self setHot:kHitNone];
}

- (void)mouseDown:(NSEvent *)event {
  _pressed = [self hitAt:[self convertPoint:event.locationInWindow fromView:nil]];
  [self setNeedsDisplay:YES];
}

- (void)mouseUp:(NSEvent *)event {
  const int released =
      [self hitAt:[self convertPoint:event.locationInWindow fromView:nil]];
  const int pressed = _pressed;
  _pressed = kHitNone;
  [self setNeedsDisplay:YES];
  // A press that wandered off its control before release is a cancel, which is
  // what every other button on the platform does.
  if (pressed == kHitNone || pressed != released) {
    return;
  }
  [self activate:released];
}

- (void)activate:(int)hit {
  switch (hit) {
  case kHitPower:
    [self queueKind:kActionSetActive value:(_active ? 0 : 1) insertId:{}];
    return;
  case kHitPresetPrev:
    if (_hasPresets) {
      [self queueKind:kActionStepPreset value:-1 insertId:{}];
    }
    return;
  case kHitPresetNext:
    if (_hasPresets) {
      [self queueKind:kActionStepPreset value:1 insertId:{}];
    }
    return;
  case kHitPresetSave:
    [self queueKind:kActionSavePreset value:0 insertId:{}];
    return;
  case kHitPresetTrigger:
    [self showPresetMenu];
    return;
  default:
    break;
  }
  if (hit >= kHitTabBase) {
    const size_t index = (size_t)((hit - kHitTabBase) / 2);
    if (index >= _tabs.size()) {
      return;
    }
    const BOOL closing = ((hit - kHitTabBase) % 2) == 1;
    [self queueKind:(closing ? kActionCloseTab : kActionSelectTab)
              value:0
           insertId:_tabs[index].insert_id];
  }
}

/// The preset list as an `NSMenu`.
///
/// The GPUI chrome opens a window of its own for this because a native child
/// window is opaque to anything GPUI would draw over it. Here the platform
/// already has the right control: a menu scrolls when it is long, takes the
/// keyboard, and dismisses the way every other menu on the machine does.
- (void)showPresetMenu {
  if (_presets.empty()) {
    return;
  }
  NSMenu *menu = [[NSMenu alloc] initWithTitle:@"Presets"];
  menu.autoenablesItems = NO;
  for (size_t i = 0; i < _presets.size(); ++i) {
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:ns(_presets[i])
                                                  action:@selector(presetPicked:)
                                           keyEquivalent:@""];
    item.target = self;
    item.tag = (NSInteger)i;
    item.enabled = YES;
    item.state = ((int)i == _presetIndex) ? NSControlStateValueOn
                                          : NSControlStateValueOff;
    [menu addItem:item];
  }
  // Anchored to the trigger's bottom-left, so the list hangs off the control
  // that opened it rather than off the pointer.
  const NSPoint anchor =
      NSMakePoint(NSMinX(_triggerRect), NSMaxY(_triggerRect) + 2.0);
  [menu popUpMenuPositioningItem:nil atLocation:anchor inView:self];
}

- (void)presetPicked:(NSMenuItem *)item {
  if (!item) {
    return;
  }
  [self queueKind:kActionSelectPreset value:(int)item.tag insertId:{}];
}

@end

// ── The container ────────────────────────────────────────────────────────────

@interface DauxEditorShellView : NSView
@property(nonatomic, strong) DauxEditorChromeView *chrome;
/// The view a plug-in attaches into. Its frame is this view's business, not the
/// plug-in's and not the bridge's — see `-layoutBands`.
@property(nonatomic, strong) NSView *pluginContainer;
@end

@implementation DauxEditorShellView

- (BOOL)isFlipped {
  return YES;
}

- (instancetype)initWithFrame:(NSRect)frame {
  self = [super initWithFrame:frame];
  if (self) {
    self.wantsLayer = YES;
    _chrome = [[DauxEditorChromeView alloc]
        initWithFrame:NSMakeRect(0, 0, frame.size.width,
                                 kTabStripHeight + kChromeRowHeight)];
    [self addSubview:_chrome];

    _pluginContainer = [[NSView alloc] initWithFrame:[self pluginArea]];
    _pluginContainer.wantsLayer = YES;
    [self addSubview:_pluginContainer];
    [self layoutBands];
  }
  return self;
}

/// Where the plug-in's own view goes: everything under the chrome.
- (NSRect)pluginArea {
  const CGFloat top = [_chrome chromeHeight];
  return NSMakeRect(0, top, self.bounds.size.width,
                    MAX(self.bounds.size.height - top, 0.0));
}

/// Put both bands back where they belong.
///
/// Deliberately re-asserted rather than left to autoresizing: a plug-in bridge
/// that writes the container's frame after attaching — which is exactly how the
/// strip once ended up underneath a VST2 editor — is corrected here on the next
/// layout instead of silently winning.
- (void)layoutBands {
  _chrome.frame =
      NSMakeRect(0, 0, self.bounds.size.width, [_chrome chromeHeight]);
  _pluginContainer.frame = [self pluginArea];
}

/// Keep the plug-in's area the size it already had after the strip changed
/// height.
///
/// An editor with no insert behind it drops the control row, and the window has
/// to give that 26 points back rather than leave a gap under the plug-in — the
/// plug-in's own size is the one thing a resize must never change.
- (void)adoptChromeHeight:(CGFloat)previousHeight {
  const CGFloat height = [_chrome chromeHeight];
  const CGFloat delta = height - previousHeight;
  if (delta == 0.0 || self.window == nil) {
    [self layoutBands];
    return;
  }
  NSRect frame = self.window.frame;
  frame.size.height += delta;
  // AppKit grows upward, so the top edge is what stays put.
  frame.origin.y -= delta;
  [self.window setFrame:frame display:YES];
  [self layoutBands];
}

- (void)resizeSubviewsWithOldSize:(NSSize)oldSize {
  [super resizeSubviewsWithOldSize:oldSize];
  [self layoutBands];
}

@end

// ── The whole window ────────────────────────────────────────────────────────

namespace {

/// The shell inside a window, or nil for a window that is not one of ours.
DauxEditorShellView *shell_of(NSWindow *window) {
  if (![window.contentView isKindOfClass:[DauxEditorShellView class]]) {
    return nil;
  }
  return (DauxEditorShellView *)window.contentView;
}

/// Smallest plug-in area we will build a window around. A plug-in that reports
/// nothing usable still gets a window it can be seen in.
constexpr CGFloat kMinPluginSide = 64.0;

NSSize sane_plugin_size(NSSize size) {
  return NSMakeSize(MAX(size.width, kMinPluginSide),
                    MAX(size.height, kMinPluginSide));
}

} // namespace

NSWindow *sphere_daux_editor_window_create(NSSize plugin_size, NSString *title,
                                           BOOL resizable,
                                           id<NSWindowDelegate> delegate) {
  const NSSize size = sane_plugin_size(plugin_size);
  const CGFloat chrome_h = kTabStripHeight + kChromeRowHeight;
  const NSRect content =
      NSMakeRect(0.0, 0.0, size.width, size.height + chrome_h);

  NSWindowStyleMask style = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                            NSWindowStyleMaskMiniaturizable;
  if (resizable) {
    style |= NSWindowStyleMaskResizable;
  }
  NSWindow *window = [[NSWindow alloc] initWithContentRect:content
                                                 styleMask:style
                                                   backing:NSBackingStoreBuffered
                                                     defer:NO];
  window.title = title.length > 0 ? title : @"Plug-in Editor";
  window.releasedWhenClosed = NO;
  // Above the studio, like every other plug-in editor on the platform: the
  // window belongs to a helper process, so ordinary activation would let the
  // app the user is actually working in cover it.
  window.level = NSFloatingWindowLevel;
  if (delegate != nil) {
    window.delegate = delegate;
  }
  window.contentView = [[DauxEditorShellView alloc] initWithFrame:content];
  [window center];
  return window;
}

NSView *sphere_daux_editor_window_plugin_container(NSWindow *window) {
  return shell_of(window).pluginContainer;
}

NSSize sphere_daux_editor_window_plugin_size(NSWindow *window) {
  DauxEditorShellView *shell = shell_of(window);
  if (shell == nil) {
    return NSZeroSize;
  }
  return [shell pluginArea].size;
}

void sphere_daux_editor_window_set_plugin_size(NSWindow *window,
                                               NSSize plugin_size) {
  DauxEditorShellView *shell = shell_of(window);
  if (shell == nil) {
    return;
  }
  const NSSize size = sane_plugin_size(plugin_size);
  const CGFloat chrome_h = kTabStripHeight + kChromeRowHeight;
  const NSRect old_frame = window.frame;
  NSRect frame = [window
      frameRectForContentRect:NSMakeRect(0.0, 0.0, size.width,
                                         size.height + chrome_h)];
  // AppKit screen coordinates grow upward. Keep the title bar where it is so a
  // plug-in resizing itself never walks the window up the display.
  frame.origin.x = old_frame.origin.x;
  frame.origin.y = NSMaxY(old_frame) - frame.size.height;
  [window setFrame:frame display:YES];

  [shell layoutBands];
  // The plug-in's own views fill the container. It put them there; nothing
  // says it will resize them, and a view left at the old size is a plug-in
  // that redraws into part of its window.
  for (NSView *child in shell.pluginContainer.subviews) {
    child.frame = NSMakeRect(0.0, 0.0, size.width, size.height);
  }
}

// ── C entry points ───────────────────────────────────────────────────────────

namespace {

/// The chrome view inside an editor window, or nil when there is none.
///
/// Reached through the window rather than through a processor: each format
/// bridge has a processor type of its own and none of them can name the
/// others', but every one of them creates an `NSWindow` for its editor. That
/// window is what the strip belongs to, so that is what addresses it.
///
/// Also nil for a window that has no shell in it, which is what a caller with
/// an editor open in some other kind of window gets — a no-op, not an error.
NSWindow *window_of(unsigned long long native_window) {
  if (native_window == 0 || !NSThread.isMainThread) {
    return nil;
  }
  return (__bridge NSWindow *)reinterpret_cast<void *>(
      static_cast<std::uintptr_t>(native_window));
}

DauxEditorChromeView *chrome_for(unsigned long long native_window) {
  NSWindow *window = window_of(native_window);
  if (window == nil) {
    return nil;
  }
  NSView *content = window.contentView;
  if (![content isKindOfClass:[DauxEditorShellView class]]) {
    return nil;
  }
  return ((DauxEditorShellView *)content).chrome;
}

/// Write the editor window's content to a PNG, for looking at it without a
/// running studio.
///
/// Gated on `FUTUREBOARD_PLUGIN_CHROME_SNAPSHOT`, which names the file. Off by
/// default and read once.
///
/// Renders the whole shell — chrome *and* the plug-in's area — rather than the
/// strip alone, and that is the point. A snapshot of the strip by itself proves
/// only that it was updated and can draw; it says nothing about whether it is
/// visible in the window, which is exactly how a VST2 editor sitting on top of
/// the chrome passed every check that existed.
void snapshot_chrome(DauxEditorChromeView *chrome) {
  static const char *path = nullptr;
  static bool resolved = false;
  if (!resolved) {
    resolved = true;
    path = std::getenv("FUTUREBOARD_PLUGIN_CHROME_SNAPSHOT");
  }
  if (!path || !*path || !chrome) {
    return;
  }
  // The shell, not the strip: a plug-in view over the chrome has to show up
  // here, and it cannot if the strip is rendered on its own.
  NSView *target = chrome.superview ? chrome.superview : (NSView *)chrome;
  NSBitmapImageRep *rep =
      [target bitmapImageRepForCachingDisplayInRect:target.bounds];
  if (!rep) {
    return;
  }
  [target cacheDisplayInRect:target.bounds toBitmapImageRep:rep];
  NSData *png = [rep representationUsingType:NSBitmapImageFileTypePNG
                                  properties:@{}];
  NSString *file = [NSString stringWithUTF8String:path];
  if (png && file) {
    [png writeToFile:file atomically:YES];
    std::fprintf(stderr, "[PluginEditorChrome] snapshot written to %s\n", path);
  }
}

} // namespace

NSView *sphere_daux_editor_shell_create(NSRect frame) {
  return [[DauxEditorShellView alloc] initWithFrame:frame];
}

NSRect sphere_daux_editor_shell_plugin_area(NSView *shell) {
  if (![shell isKindOfClass:[DauxEditorShellView class]]) {
    return shell ? shell.bounds : NSZeroRect;
  }
  return [(DauxEditorShellView *)shell pluginArea];
}

extern "C" {

double sphere_daux_editor_chrome_height(void) {
  return kTabStripHeight + kChromeRowHeight;
}

void sphere_daux_editor_chrome_begin(unsigned long long native_window) {
  DauxEditorChromeView *chrome = chrome_for(native_window);
  [chrome beginUpdate];
  [chrome resetPresetIndex];
}



void sphere_daux_editor_chrome_set_header(unsigned long long native_window,
                                          int active, const char *preset_label,
                                          const char *cpu_label,
                                          const char *latency_label,
                                          const char *active_tab,
                                          int shows_controls) {
  [chrome_for(native_window) setHeaderActive:(active != 0)
                                 presetLabel:preset_label
                                    cpuLabel:cpu_label
                                latencyLabel:latency_label
                                   activeTab:active_tab
                               showsControls:(shows_controls != 0)];
}

void sphere_daux_editor_chrome_add_preset(unsigned long long native_window,
                                          const char *name, int selected) {
  [chrome_for(native_window) addPreset:name selected:(selected != 0)];
}

void sphere_daux_editor_chrome_add_tab(unsigned long long native_window,
                                       const char *insert_id,
                                       const char *display_name,
                                       int insert_number) {
  [chrome_for(native_window) addTab:insert_id
                              name:display_name
                            number:insert_number];
}

void sphere_daux_editor_chrome_set_palette(unsigned long long native_window,
                                           const unsigned int *colors,
                                           int count) {
  [chrome_for(native_window) setPalette:colors count:count];
}

void sphere_daux_editor_chrome_commit(unsigned long long native_window,
                                      const char *window_title) {
  DauxEditorChromeView *chrome = chrome_for(native_window);
  if (!chrome) {
    std::fprintf(stderr,
                 "[PluginEditorChrome] update dropped: no editor window open\n");
    return;
  }
  const CGFloat height_before = [chrome heightAtUpdateStart];
  [chrome commitUpdate];
  DauxEditorShellView *shell = shell_of(window_of(native_window));
  if (shell != nil) {
    // The strip may have gained or lost its control row; the window gives the
    // difference back to the plug-in rather than leaving a gap.
    [shell adoptChromeHeight:height_before];
  }
  [chrome displayIfNeeded];
  std::fprintf(stderr, "[PluginEditorChrome] applied tabs=%d presets=%d\n",
               (int)[chrome tabCount], (int)[chrome presetCount]);
  snapshot_chrome(chrome);
  if (window_title && *window_title) {
    NSString *title = [NSString stringWithUTF8String:window_title];
    if (title) {
      NSWindow *window = (__bridge NSWindow *)reinterpret_cast<void *>(
          static_cast<std::uintptr_t>(native_window));
      [window setTitle:title];
    }
  }
}

int sphere_daux_editor_chrome_take_action(unsigned long long native_window,
                                          int *out_kind, int *out_value,
                                          char *out_id, int out_id_capacity) {
  DauxEditorChromeView *chrome = chrome_for(native_window);
  if (!chrome) {
    return 0;
  }
  return [chrome takeAction:out_kind
                      value:out_value
                         id:out_id
                   capacity:out_id_capacity]
             ? 1
             : 0;
}

} // extern "C"
