/* MSVC lacks the POSIX <strings.h> that litehtml's vendored gumbo parser includes. */
#pragma once
#include <string.h>

#ifndef strcasecmp
#define strcasecmp _stricmp
#endif
#ifndef strncasecmp
#define strncasecmp _strnicmp
#endif
