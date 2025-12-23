"""
Basic import tests for TokioLoop to ensure it can be imported alongside RLoop.
"""



def test_import_rloop():
    """Test that RLoop can be imported."""
    from rloop import RLoop
    assert RLoop is not None
    assert hasattr(RLoop, 'run_forever')
    assert hasattr(RLoop, 'call_soon')


def test_import_tokio_loop():
    """Test that TokioLoop can be imported."""
    from rloop import TokioLoop
    assert TokioLoop is not None
    assert hasattr(TokioLoop, 'run_forever')
    assert hasattr(TokioLoop, 'call_soon')


def test_import_both_loops():
    """Test that both RLoop and TokioLoop can be imported together."""
    from rloop import RLoop, TokioLoop

    # Both should be importable
    assert RLoop is not None
    assert TokioLoop is not None

    # Both should have the same basic interface
    assert hasattr(RLoop, 'run_forever')
    assert hasattr(TokioLoop, 'run_forever')
    assert hasattr(RLoop, 'call_soon')
    assert hasattr(TokioLoop, 'call_soon')


def test_new_event_loop_functions():
    """Test that both factory functions are available."""
    from rloop import new_event_loop, new_tokio_event_loop

    # Both should be callable
    assert callable(new_event_loop)
    assert callable(new_tokio_event_loop)

    # Both should return the correct types
    rloop = new_event_loop()
    tokioloop = new_tokio_event_loop()

    from rloop import RLoop, TokioLoop
    assert isinstance(rloop, RLoop)
    assert isinstance(tokioloop, TokioLoop)


def test_policies():
    """Test that both event loop policies are available."""
    from rloop import EventLoopPolicy, TokioEventLoopPolicy

    # Both should be importable
    assert EventLoopPolicy is not None
    assert TokioEventLoopPolicy is not None

    # Both should have the _loop_factory method
    assert hasattr(EventLoopPolicy, '_loop_factory')
    assert hasattr(TokioEventLoopPolicy, '_loop_factory')


def test_tokio_loop_basic_functionality():
    """Test that TokioLoop can be instantiated and has basic functionality."""
    from rloop import TokioLoop

    loop = TokioLoop()

    # Test basic properties
    assert not loop.is_closed()
    assert not loop.is_running()

    # Test basic methods exist
    assert hasattr(loop, 'call_soon')
    assert hasattr(loop, 'call_later')
    assert hasattr(loop, 'create_future')
    assert hasattr(loop, 'create_task')
    assert hasattr(loop, 'run_forever')
    assert hasattr(loop, 'stop')
    assert hasattr(loop, 'close')

    # Test that methods are callable
    assert callable(loop.call_soon)
    assert callable(loop.call_later)
    assert callable(loop.create_future)
    assert callable(loop.create_task)
    assert callable(loop.run_forever)
    assert callable(loop.stop)
    assert callable(loop.close)

