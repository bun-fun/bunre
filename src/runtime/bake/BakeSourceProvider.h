#pragma once
#include "root.h"
#include "headers-handwritten.h"
#include "JavaScriptCore/SourceOrigin.h"

namespace Bake {

class SourceProvider;

class SourceProvider final : public JSC::StringSourceProvider {
public:
    static Ref<SourceProvider> create(
        JSC::JSGlobalObject*,
        const String& source,
        const JSC::SourceOrigin& sourceOrigin,
        String&& sourceURL,
        const TextPosition& startPosition,
        JSC::SourceProviderSourceType sourceType)
    {
        auto provider = adoptRef(*new SourceProvider(source, sourceOrigin, WTF::move(sourceURL), startPosition, sourceType));
        return provider;
    }

private:
    SourceProvider(
        const String& source,
        const JSC::SourceOrigin& sourceOrigin,
        String&& sourceURL,
        const TextPosition& startPosition,
        JSC::SourceProviderSourceType sourceType)
        : StringSourceProvider(
              source,
              sourceOrigin,
              JSC::SourceTaintedOrigin::Untainted,
              WTF::move(sourceURL),
              startPosition,
              sourceType)
    {
    }
};

} // namespace Bake
