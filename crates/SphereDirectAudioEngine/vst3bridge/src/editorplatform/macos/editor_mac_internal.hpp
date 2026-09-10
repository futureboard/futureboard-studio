#pragma once

#import <Cocoa/Cocoa.h>

#include "../../../include/sphere_daux_editor_bridge.h"
#include "../../../include/sphere_daux_editor_chrome.h"
#include "../../../include/sphere_daux_editor_shell_mac.h"

NSColor *daux_bg_color(void);

/// Resize the NSWindow content while preserving its top-left screen position.
/// `notify_plugin` forwards the agreed content size through IPlugView::onSize.
void daux_resize_editor_content(SphereDauxVst3Processor *proc, int width,
                                int height, bool notify_plugin,
                                const char *reason);

void close_editor_mac(SphereDauxVst3Processor *proc);

/// Receives close-button clicks from the NSWindow and delegates them to the
/// processor's close path so IPlugView::removed() is called correctly.
@interface DauxEditorWindowDelegate : NSObject <NSWindowDelegate>
@property(nonatomic, assign) SphereDauxVst3Processor *processor;
@property(nonatomic, assign) BOOL applyingHostResize;
@end
