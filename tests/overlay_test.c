// Draws frames like the game (OpenGL, presented with GDI32!SwapBuffers) in a hidden window with winmm.dll loaded,
// then checks that the HUD covered the top-right corner and that the GL state the test set survived.
// Single-buffered, so the HUD lands in the buffer glReadPixels reads.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>

#define W 800
#define H 600
static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("FAIL %s:%d: ", __FILE__, __LINE__); \
    printf(__VA_ARGS__); printf("\n"); } } while (0)

int main(void) {
    char path[MAX_PATH];
    GetModuleFileNameA(NULL, path, MAX_PATH);
    strcpy(strrchr(path, '\\') + 1, "winmm.dll");
    if (!LoadLibraryA(path)) {
        printf("FAIL: cannot load winmm.dll\n");
        return 2;
    }
    WNDCLASSA wc = {0};
    wc.lpfnWndProc = DefWindowProcA;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "overlay_test";
    RegisterClassA(&wc);
    RECT rect = {0, 0, W, H};
    AdjustWindowRect(&rect, WS_OVERLAPPEDWINDOW, FALSE);
    HWND window = CreateWindowA("overlay_test", "overlay_test", WS_OVERLAPPEDWINDOW, 0, 0, rect.right - rect.left,
        rect.bottom - rect.top, NULL, NULL, wc.hInstance, NULL);
    HDC dc = GetDC(window);
    PIXELFORMATDESCRIPTOR pfd = {sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC context = wglCreateContext(dc);
    if (!context || !wglMakeCurrent(dc, context)) {
        printf("SKIP overlay: no OpenGL context\n");
        return 0;
    }
    printf("  OpenGL %s\n", (const char *)glGetString(GL_VERSION));
    for (int frame = 0; frame < 10; frame++) {
        glViewport(0, 0, W, H);
        glDisable(GL_BLEND);
        glClearColor(0.2f, 0.4f, 0.6f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glViewport(10, 20, 300, 200); // state the game leaves behind
        SwapBuffers(dc);
        glFinish();
        Sleep(30);
    }
    unsigned char corner[4], middle[4];
    glReadPixels(W - 20, H - 20, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, corner);
    glReadPixels(W / 2, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, middle);
    GLint viewport[4];
    glGetIntegerv(GL_VIEWPORT, viewport);
    GLint program;
    glGetIntegerv(0x8B8D, &program);
    CHECK(middle[2] > 140 && middle[0] < 70, "middle of the frame changed: %d,%d,%d", middle[0], middle[1], middle[2]);
    CHECK(corner[2] < 120, "no HUD in the top-right corner: %d,%d,%d", corner[0], corner[1], corner[2]);
    CHECK(viewport[0] == 10 && viewport[1] == 20 && viewport[2] == 300, "viewport not restored: %d,%d,%d",
        viewport[0], viewport[1], viewport[2]);
    CHECK(!glIsEnabled(GL_BLEND), "blending left on");
    CHECK(program == 0, "shader program %d left bound", program);
    if (getenv("OVERLAY_DUMP")) { // raw RGBA of the top-right 420x160, bottom row first
        static unsigned char shot[420 * 160 * 4];
        glReadPixels(W - 420, H - 160, 420, 160, GL_RGBA, GL_UNSIGNED_BYTE, shot);
        FILE *file = fopen(getenv("OVERLAY_DUMP"), "wb");
        if (file) {
            fwrite(shot, 1, sizeof shot, file);
            fclose(file);
        }
    }
    printf("overlay: %s\n", failures ? "FAILED" : "ok");
    return failures ? 1 : 0;
}
