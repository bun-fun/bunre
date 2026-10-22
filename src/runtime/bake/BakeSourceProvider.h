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

    // The Bun VM this provider was created in; its source-map lookups go through that VM's bake global rather than the
    // calling thread's (a stack trace can be remapped from the collector thread).
    void* bunVM() const { return m_bunVM; }

private:
    SourceProvider(
        void* bunVM,
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
        , m_bunVM(bunVM)
    {
    }

    // Runs during JSC's sweep, when the global object may already be swept, so use the saved `m_bunVM`.
    ~SourceProvider()
    {
        auto specifier = Bun::toString(sourceURL());
        Bun__removeBakeSourceProviderSourceMap(m_bunVM, this, &specifier);
    }

    void* m_bunVM;
};

} // namespace Bake
