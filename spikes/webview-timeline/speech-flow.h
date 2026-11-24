// Process-wide passive speech evidence; no text, audio or synthesized callbacks.
static NSMutableArray *speechFlowEvents;
static NSMutableDictionary *speechFlowUtterances;
static NSUInteger speechFlowReceived, speechFlowFaults;
static NSDictionary *speechFlowKinds(void) {
    return @{@"speak/call":@"request", @"speak/return":@"returned", @"speak/threw":@"failed",
        @"stop/call":@"stop-request", @"stop/return":@"stop-returned",
        @"pause/call":@"pause-request", @"pause/return":@"pause-returned",
        @"resume/call":@"resume-request", @"resume/return":@"resume-returned",
        @"delegate/playing":@"started", @"delegate/done":@"finished",
        @"delegate/stopped":@"cancelled", @"delegate/paused":@"paused",
        @"delegate/resumed":@"resumed"};
}
static void recordSpeechFlow(NSNumber *synthesizer, NSString *operation, NSString *phase,
                             NSDictionary *extra, BOOL speaking, BOOL paused) {
    NSString *kind = speechFlowKinds()[[NSString stringWithFormat:@"%@/%@", operation, phase]];
    if (!kind) return;
    if (!speechFlowEvents) speechFlowEvents = [NSMutableArray new];
    if (!speechFlowUtterances) speechFlowUtterances = [NSMutableDictionary new];
    id utterance = extra[@"utterance_id"] ?: NSNull.null;
    NSDictionary *event = @{@"sequence":@(++speechFlowReceived), @"synthesizer":synthesizer,
        @"utterance":utterance, @"kind":kind, @"speaking":@(speaking), @"paused":@(paused),
        @"accepted":extra[@"accepted"] ?: NSNull.null,
        @"implementedCallbacks":extra[@"implemented_callbacks"] ?: NSNull.null};
    [speechFlowEvents addObject:event];
    if (speechFlowEvents.count > 32) [speechFlowEvents removeObjectAtIndex:0];
    if ([utterance isKindOfClass:NSNumber.class]) {
        NSMutableDictionary *summary = speechFlowUtterances[utterance];
        if (!summary) {
            summary = [@{@"id":utterance, @"requests":@0, @"starts":@0,
                @"finishes":@0, @"cancellations":@0, @"failures":@0, @"implementedCallbacks":NSNull.null} mutableCopy];
            speechFlowUtterances[utterance] = summary;
        }
        NSString *counter = @{@"request":@"requests", @"started":@"starts",
            @"finished":@"finishes", @"cancelled":@"cancellations", @"failed":@"failures"}[kind];
        if (counter) summary[counter] = @([summary[counter] unsignedIntegerValue] + 1);
        if (extra[@"implemented_callbacks"]) summary[@"implementedCallbacks"] = extra[@"implemented_callbacks"];
        summary[@"lastSequence"] = @(speechFlowReceived);
        summary[@"lastKind"] = kind;
    }
}
#ifdef FRONTCAP_SPEECH_DELEGATE_LINKED
BOOL speechDelegateObservationAvailable(void);
#else
__attribute__((weak)) BOOL speechDelegateObservationAvailable(void) { return NO; }
#endif
NSDictionary *speechObservationSnapshot(void) {
    @synchronized(AVSpeechSynthesizer.class) {
        static NSString *process;
        if (!process) process = NSUUID.UUID.UUIDString.lowercaseString;
        NSMutableArray *summaries = [NSMutableArray new];
        for (NSNumber *identity in [[speechFlowUtterances allKeys] sortedArrayUsingSelector:@selector(compare:)])
            [summaries addObject:[speechFlowUtterances[identity] copy]];
        NSMutableArray *current = [NSMutableArray new];
        NSUInteger omitted = 0;
        for (id synthesizer in synthesizers.keyEnumerator) {
            if (current.count >= 8) { omitted++; continue; }
            [current addObject:@{@"id":[synthesizers objectForKey:synthesizer],
                @"speaking":@([synthesizer isSpeaking]), @"paused":@([synthesizer isPaused])}];
        }
        return @{@"process":process, @"pid":@(NSProcessInfo.processInfo.processIdentifier),
            @"apiObserved":@((BOOL)(originalSpeak != NULL)),
            @"delegateObserved":@(speechDelegateObservationAvailable()),
            @"received":@(speechFlowReceived), @"dropped":@(speechFlowReceived - speechFlowEvents.count),
            @"truncated":@((BOOL)(sequence >= 512)), @"faults":@(speechFlowFaults),
            @"utteranceLimitReached":@((BOOL)(nextSpeechUtterance >= 64)),
            @"events":[speechFlowEvents copy] ?: @[], @"utterances":summaries,
            @"synthesizers":current, @"omittedSynthesizers":@(omitted)};
    }
}
