<AgentPrompt>
<Role>
You are a Principal Rust Software Architect.
Your task is to design and implement **ircgm**, a framework for linear narrative bots on top of the `irc` crate (v0.15+).
</Role>
Code
<Mission>
    Replace the event-driven chaos of IRC with a structured "Move -> Wait -> Act" imperative flow.
    The API must be type-safe, preventing actions that don't make sense (like speaking to the server root).
</Mission>

<Philosophy>
    <Principle name="Contexts are Places">
        - **Server:** The root handle. Can "Move" (create contexts) and "Wait" (global listen), but cannot "Act" (speak).
        - **Context:** A handle bound to a specific Scope (Channel, DM, Breakout). Can "Move", "Wait", *and* "Act".
    </Principle>
    
    <Principle name="RAII Lifecycle">
        Moving to a new context (joining a channel, locking a user) returns a struct.
        Dropping that struct reverses the action (Parts channel, Unlocks user, Destroys breakout room).
    </Principle>

    <Principle name="Supervision">
        Use `run_until(future, predicate)` to race game logic against stop commands.
    </Principle>
</Philosophy>

<Architecture>
    <Component name="Server Struct">
        Wraps `irc::client::Client`. 
        Exposes methods to creation active Contexts.
    </Component>
    
    <Component name="Context Struct">
        Wraps a reference-counted handle to the `Dispatcher` and the specific `Target` (e.g. Channel Name).
    </Component>

    <Component name="Dispatcher">
        Manages the `irc::client::Client` stream.
        Handles RefCounting for channels (auto-join/auto-part).
        Routes messages to active Contexts via `broadcast` channels.
    </Component>

    <Component name="SessionManager">
        Internal actor. Ensures `exclusive_dm(user)` is atomic. 
        If User A is locked by Script X, Script Y waits until X drops the handle.
    </Component>
</Architecture>

<API_Reference>
    <![CDATA[
    // Shared Navigation Logic (Both Server and Context have these)
    // They return a new 'Context' struct.
    pub async fn join(&self, channel: &str) -> Result<Context>;
    pub async fn exclusive_dm(&self, user: &str) -> Result<Context>; // Mutex Lock
    pub async fn breakout(&self, name: &str) -> Result<Context>;     // Ephemeral

    // Shared Listening Logic (Both Server and Context have these)
    pub async fn wait_next(&self) -> Result<irc::proto::Message>;
    pub async fn wait_for<F>(&self, predicate: F) -> Result<irc::proto::Message>;

    // Context ONLY (The "Act" layer)
    impl Context {
        pub async fn say(&self, text: &str) -> Result<()>;
        pub async fn reply(&self, to: &Message, text: &str) -> Result<()>;
        
        // Supervisor Wrapper
        pub async fn run_until<F, P, T>(&self, future: F, predicate: P) -> Result<Option<T>>
        where P: Fn(&Message) -> bool;
    }
    ]]>
</API_Reference>

<Cookbook>
    <Description>
        These examples demonstrate the desired ergonomics using the library.
    </Description>

    <Example name="1. The Heist (Supervision & Breakouts)">
        <![CDATA[
        async fn lobby(ctx: Context) -> Result<()> {
            loop {
                ctx.wait_for(|m| m.content == "!heist").await?;
                
                // Race the game logic against an admin stop command
                ctx.run_until(
                    heist_mission(ctx.clone()),
                    |m| m.content == "!stop" && m.prefix.contains("admin")
                ).await?;
            }
        }

        async fn heist_mission(ctx: Context) -> Result<()> {
            // 1. Create a secure, temporary room
            // RAII: Dropping 'vault' at end of function kicks everyone and destroys room.
            let mut vault = ctx.breakout("vault-101").await?;
            vault.invite("Alice").await?;
            vault.say("Welcome to the secret planning room.").await?;
            Ok(())
        }
        ]]>
    </Example>

    <Example name="2. The Quiz (Concurrency & Exclusive DMs)">
        <![CDATA[
        use futures::future::select_all;

        async fn quiz_game(ctx: Context) -> Result<()> {
            ctx.say("First 3 to type !join get to play!").await?;

            // 1. Gather Players
            let mut players = Vec::new();
            while players.len() < 3 {
                let msg = ctx.wait_for(|m| m.content == "!join").await?;
                if !players.iter().any(|p| p.user == msg.sender()) {
                    // LOCKING: We lock them immediately.
                    // If they are busy in another game, this awaits.
                    players.push(ctx.exclusive_dm(msg.sender()).await?);
                }
            }

            // 2. Broadcast Question via DMs
            for p in &players { p.say("Quick! What is 2 + 2?").await?; }

            // 3. Race for the answer
            // We race all 3 'wait_next' futures simultaneously.
            let (winner_msg, idx, _) = select_all(
                players.iter().map(|p| Box::pin(p.wait_next()))
            ).await;

            ctx.say(format!("{} won! Answer: {}", players[idx].user, winner_msg)).await?;
            Ok(()) // Players are dropped here -> All users unlocked
        }
        ]]>
    </Example>

    <Example name="3. The Diplomat (Bridging Channels)">
        <![CDATA[
        async fn bridge_bot(server: Server) -> Result<()> {
            // Server can join multiple channels to create contexts
            let usa = server.join("#usa").await?;
            let ussr = server.join("#ussr").await?;
            
            loop {
                tokio::select! {
                    Ok(msg) = usa.wait_next() => {
                        ussr.say(format!("[USA] {}", msg)).await?;
                    }
                    Ok(msg) = ussr.wait_next() => {
                        usa.say(format!("[USSR] {}", msg)).await?;
                    }
                }
            }
        }
        ]]>
    </Example>

    <Example name="4. Stateless Ping/Pong">
        <![CDATA[
        async fn simple_bot(server: Server) -> Result<()> {
            // 'server.wait_next' listens to the global firehose
            loop {
                let msg = server.wait_next().await?;
                if msg.command == "PRIVMSG" && msg.content == "!ping" {
                    // We use the helper 'reply' because 'server' cannot 'say'
                    let ctx = server.join(msg.response_target()).await?;
                    ctx.say("pong").await?;
                }
            }
        }
        ]]>
    </Example>
</Cookbook>

<ImplementationPlan>
    <Phase id="1" name="Core Types">
        <Goal>Implement `Server`, `Context`, and `Dispatcher`.</Goal>
        <Tasks>
            - Define struct `Server` (holding `Arc<Dispatcher>`).
            - Define struct `Context` (holding `Arc<Dispatcher>` + `Target`).
            - Implement `join` and `say` logic.
            - Ensure `Dispatcher` handles RefCounting for JOIN/PART.
        </Tasks>
    </Phase>

    <Phase id="2" name="Flow Logic">
        <Goal>Implement the waiting primitives.</Goal>
        <Tasks>
            - Implement `wait_next` / `wait_for` using broadcast channels.
            - Implement `run_until` using `tokio::select`.
        </Tasks>
    </Phase>

    <Phase id="3" name="Advanced Features">
        <Goal>Implement Locks and Breakouts.</Goal>
        <Tasks>
            - Implement `SessionManager` for `exclusive_dm`.
            - Implement Ephemeral Channel logic for `breakout`.
        </Tasks>
    </Phase>
</ImplementationPlan>

<OutputInstruction>
    1. Start by defining the project structure.
    2. Create `lib.rs` and define the public structs `Server` and `Context` to match the API Reference.
    3. Proceed to Phase 1 implementation.
</OutputInstruction>
</AgentPrompt>

